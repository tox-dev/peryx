//! Driving the router: request helpers, the upstream mocks, and the multipart upload builders.

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
/// A detail page whose wheel advertises a PEP 658 `core-metadata` sibling by default, so serving it
/// registers the sibling and spawns no metadata backfill. That detached backfill fetches the wheel
/// from upstream to read its `METADATA` member; under load it races the mock windows these tests set
/// up, tripping verify-on-drop `.expect(N)` counts. Tests that mean to exercise the no-metadata or
/// backfill path build their own page or seed state directly rather than call this helper.
pub fn detail_json(digest: &str, file_url: &str) -> String {
    let metadata = Digest::of(b"flask metadata");
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
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
        "{{\"meta\":{{\"api-version\":\"1.4\",\"project-status\":\"{status}\",\
         \"project-status-reason\":\"{reason}\"}},\"name\":\"{project}\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"{project}-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path(format!("/simple/{project}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(server)
        .await;
}
pub fn range_response(bytes: Vec<u8>) -> impl wiremock::Respond {
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
            .insert_header("content-range", format!("bytes {start}-{end}/{}", bytes.len()))
            .set_body_bytes(bytes[start..=end].to_vec())
    }
}
pub async fn assert_metadata_range_fallback(
    h: &Harness,
    label: &str,
    ranged: Vec<u8>,
    wheel: Vec<u8>,
    metadata: &[u8],
) {
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", ranged.len()),
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
    post_upload_body_response(state, uri, auth, content_type, Body::from(body)).await
}
pub async fn post_upload_body_response(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    body: Body,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, content_type);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
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
