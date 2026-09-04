use super::*;

pub async fn get(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let (status, headers, bytes) = get_bytes(state, uri, accept).await;
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}
pub async fn get_with_headers(
    state: &Arc<AppState>,
    uri: &str,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let (status, _, bytes) = get_bytes_with_headers(state, uri, extra_headers).await;
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
pub async fn get_bytes(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let accept = accept.map(|accept| (header::ACCEPT.as_str(), accept));
    get_bytes_with_headers(state, uri, accept.as_slice()).await
}
pub async fn get_bytes_with_headers(
    state: &Arc<AppState>,
    uri: &str,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    send_bytes(state, "GET", uri, extra_headers).await
}
pub async fn send_bytes(
    state: &Arc<AppState>,
    verb: &str,
    uri: &str,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().uri(uri).method(verb);
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

pub async fn request_from_peer(
    state: &Arc<AppState>,
    verb: &str,
    uri: &str,
    auth: Option<&str>,
    peer: &str,
    forwarded_for: &str,
) -> StatusCode {
    let mut builder = Request::builder()
        .uri(uri)
        .method(verb)
        .header("x-forwarded-for", forwarded_for);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 51_000)));
    router(state.clone()).oneshot(request).await.unwrap().status()
}

pub async fn request(state: &Arc<AppState>, verb: &str, uri: &str, auth: Option<&str>) -> StatusCode {
    request_response(state, verb, uri, auth).await.0
}
pub async fn request_response(
    state: &Arc<AppState>,
    verb: &str,
    uri: &str,
    auth: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(uri).method(verb);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
/// Register `filename`/`digest` as published by `index` at `url`, the way a fetched page does. The
/// file route releases bytes only for a pair the addressed index publishes, so a test that seeds a
/// download by hand seeds the publication with it.
pub fn publish_file(state: &AppState, index: &str, filename: &str, digest: &Digest, url: &str) {
    crate::tests::register_publication(&state.serving.meta, index, filename, digest.as_str(), None);
    state
        .serving
        .meta
        .put_file_url(
            index,
            &crate::project_of_filename(filename),
            digest.as_str(),
            url,
            index,
        )
        .unwrap();
}
pub fn revoke_digest(state: &AppState, digest: &Digest) {
    state
        .serving
        .put_digest_revocation(
            &ArtifactDigest::from_sha256(digest.as_str()).unwrap(),
            &RevocationReason::new("compromised builder").unwrap(),
            &UserId::random(),
        )
        .unwrap();
}
pub fn lift_digest(state: &AppState, digest: &Digest) {
    state
        .serving
        .lift_digest_revocation(
            &ArtifactDigest::from_sha256(digest.as_str()).unwrap(),
            &UserId::random(),
        )
        .unwrap();
}
// The default PEP 658 sibling prevents backfill from racing request-count assertions.
pub fn detail_json(digest: &str, file_url: &str) -> String {
    let metadata = Digest::of(b"flask metadata");
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"core-metadata\":{{\"sha256\":\"{metadata}\"}}}}]}}",
        metadata = metadata.as_str(),
    )
}
pub async fn mount_detail(server: &MockServer, digest: &str, file_url: &str, etag: Option<&str>) {
    let mut response = ResponseTemplate::new(200).set_body_raw(
        detail_json(digest, file_url).into_bytes(),
        "application/vnd.pypi.simple.v1+json",
    );
    if let Some(etag) = etag {
        response = response.insert_header("etag", etag);
    }
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(response)
        .mount(server)
        .await;
}
pub async fn mount_status_detail(
    server: &MockServer,
    project: &str,
    status: &str,
    reason: &str,
    digest: &str,
    file_url: &str,
) {
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\
         \"project-status\":{{\"status\":\"{status}\",\"reason\":\"{reason}\"}},\
         \"name\":\"{project}\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"{project}-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path(format!("/simple/{project}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(server)
        .await;
}
/// Ranged reads pin one representation, so mocked `HEAD` and range responses share one validator.
pub const WHEEL_ETAG: &str = "\"wheel-generation\"";

pub fn range_response(bytes: Vec<u8>) -> impl Fn(&wiremock::Request) -> ResponseTemplate + Send + Sync {
    move |request: &wiremock::Request| {
        let Some(range) = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
        else {
            return ResponseTemplate::new(416);
        };
        let Some((start, end)) = range.split_once('-') else {
            return ResponseTemplate::new(416);
        };
        let (Some(start), Some(end)) = (start.parse::<usize>().ok(), end.parse::<usize>().ok()) else {
            return ResponseTemplate::new(416);
        };
        if start > end || end >= bytes.len() {
            return ResponseTemplate::new(416);
        }
        ResponseTemplate::new(206)
            .insert_header("accept-ranges", "bytes")
            .insert_header("etag", WHEEL_ETAG)
            .insert_header("content-range", format!("bytes {start}-{end}/{}", bytes.len()))
            .set_body_bytes(bytes[start..=end].to_vec())
    }
}

/// Answers the first range from the pinned generation and every later one from the next, mimicking
/// an upstream rotation in the middle of one ranged read.
pub fn rotating_range_response(bytes: Vec<u8>) -> impl wiremock::Respond {
    let pinned = range_response(bytes);
    let served = AtomicUsize::new(0);
    move |request: &wiremock::Request| {
        let response = pinned(request);
        if served.fetch_add(1, Ordering::Relaxed) == 0 {
            response
        } else {
            response.insert_header("etag", "\"next-generation\"")
        }
    }
}

#[rstest]
#[case::missing(None)]
#[case::missing_end(Some("bytes=1"))]
#[case::non_numeric(Some("bytes=x-1"))]
#[case::reversed(Some("bytes=2-1"))]
#[case::past_end(Some("bytes=0-4"))]
#[tokio::test]
async fn test_range_response_rejects_invalid_ranges(#[case] range: Option<&str>) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(range_response(b"data".to_vec()))
        .mount(&server)
        .await;
    let mut request = reqwest::Client::new().get(server.uri());
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }

    assert_eq!(
        request.send().await.unwrap().status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
}

pub async fn assert_metadata_range_fallback_preserves_other_resources(
    h: &Harness,
    label: &str,
    ranged: Vec<u8>,
    wheel: Vec<u8>,
    metadata: &[u8],
) {
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", ranged.len())
                .insert_header("etag", WHEEL_ETAG),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(ranged))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(10)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK, "{label}");
    assert_eq!(body.as_bytes(), metadata, "{label}");

    let next_metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 2.0\n";
    let next_wheel = fixture_wheel_with_body_and_metadata("2.0", b"VALUE = 2\n", Some(next_metadata));
    let next_digest = Digest::of(&next_wheel);
    let next_filename = "peryxpkg-2.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        next_filename,
        &next_digest,
        &format!("{}/files/{next_filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{next_filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", next_wheel.len())
                .insert_header("etag", WHEEL_ETAG),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{next_filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(next_wheel))
        .mount(&h.server)
        .await;

    let next_uri = format!("/pypi/files/{}/{next_filename}.metadata", next_digest.as_str());
    let (status, _, body) = get(&h.state, &next_uri, None).await;

    assert_eq!(status, StatusCode::OK, "{label}");
    assert_eq!(body.as_bytes(), next_metadata, "{label}");
}
pub fn upload_fields() -> Vec<(&'static str, &'static str)> {
    vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("requires_python", ">=3.8"),
    ]
}
pub fn multipart_body(fields: &[(&str, &str)], content: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let contents = content.into_iter().collect::<Vec<_>>();
    multipart_body_with_content_parts(fields, &contents)
}
pub fn multipart_body_with_content_parts(fields: &[(&str, &str)], contents: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "peryxtestboundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    for (filename, bytes) in contents {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{filename}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}
pub fn upload_auth() -> String {
    format!("Basic {}", STANDARD.encode("__token__:s3cret"))
}
pub async fn post_upload(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> StatusCode {
    post_upload_response(state, uri, auth, content_type, body).await.0
}
pub async fn post_upload_response(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    post_upload_body_with_headers_response(state, uri, auth, content_type, &[], Body::from(body)).await
}
pub async fn post_upload_with_headers_response(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, String) {
    post_upload_body_with_headers_response(state, uri, auth, content_type, headers, Body::from(body)).await
}
pub async fn post_upload_body_with_headers_response(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, content_type);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    for &(name, value) in headers {
        builder = builder.header(name, value);
    }
    let response = router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
pub async fn assert_upload_response(
    h: &Harness,
    fields: &[(&str, &str)],
    content: Option<(&str, &[u8])>,
    expected_status: StatusCode,
    expected_body: &str,
) {
    let (ct, body) = multipart_body(fields, content);
    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;
    assert_eq!(status, expected_status);
    assert_eq!(body, expected_body);
}
pub async fn upload_peryxpkg(state: &Arc<AppState>, uri: &str, wheel: &[u8]) -> StatusCode {
    let (ct, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", wheel)));
    post_upload(state, uri, Some(&upload_auth()), &ct, body).await
}
pub async fn upload_version(state: &Arc<AppState>, uri: &str, version: &str) -> StatusCode {
    let wheel = fixture_wheel_for(version);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", version),
        ("filetype", "bdist_wheel"),
    ];
    let filename = format!("peryxpkg-{version}-py3-none-any.whl");
    let (ct, body) = multipart_body(&fields, Some((&filename, &wheel)));
    post_upload(state, uri, Some(&upload_auth()), &ct, body).await
}
