//! The harness every HTTP-level `PyPI` serving test builds on: a wired state over temporary
//! stores and a mock upstream, the request helpers, and the wheel and page fixtures.

pub use std::collections::BTreeMap;
pub use std::fmt::Write as _;
pub use std::io::Write as _;
pub use std::path::Path;
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicI64, Ordering};

pub use crate::{CoreMetadata, File, Provenance, Yanked, to_json};
pub use axum::body::Body;
pub use axum::http::{HeaderMap, Request, StatusCode, header};
pub use base64::Engine as _;
pub use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
pub use http_body_util::BodyExt as _;
pub use peryx_storage::blob::{BlobStore, Digest};
pub use peryx_storage::meta::{CachedIndex, MetaStore};
pub use peryx_upstream::{Auth, UpstreamClient};
pub(crate) use rstest::rstest;
pub use sha2::{Digest as _, Sha256};
pub use tower::ServiceExt as _;
pub use wiremock::matchers::{header as match_header, header_regex, method, path};
pub use wiremock::{Mock, MockServer, ResponseTemplate};

pub use crate::cache;
pub use crate::tests::{LogCapture, field};
pub use crate::upload::Uploaded;
pub use peryx_core::path::local_file_url;
pub use peryx_driver::DEFAULT_MAX_STALE_SECS;
pub use peryx_driver::state::AppState;
pub use peryx_http::router;
pub use peryx_index::{Index, IndexKind};
pub use peryx_policy::{Policy, PolicyConfig};

pub use crate::policy::{PackageType, PypiPolicyConfig, compile_rules};

pub struct Harness {
    pub(crate) _dir: tempfile::TempDir,
    pub(crate) server: MockServer,
    pub(crate) state: Arc<AppState>,
    pub(crate) clock: Arc<AtomicI64>,
}

/// A cache (`pypi`) of the mock, a hosted store (`hosted`), and a virtual index (`root/pypi`) that
/// layers the hosted store in front of the cache. `token`/`volatile` tune the hosted store.
pub async fn harness_with(token: bool, volatile: bool) -> Harness {
    harness_with_policies(token, volatile, Policy::default(), Policy::default(), Policy::default()).await
}

pub async fn harness_with_policies(
    token: bool,
    volatile: bool,
    mirror_policy: Policy,
    local_policy: Policy,
    overlay_policy: Policy,
) -> Harness {
    harness_with_stale(
        token,
        volatile,
        mirror_policy,
        local_policy,
        overlay_policy,
        DEFAULT_MAX_STALE_SECS,
    )
    .await
}

/// A harness whose stale-on-error bound the caller chooses; `0` serves stale without limit.
pub async fn harness_with_stale(
    token: bool,
    volatile: bool,
    mirror_policy: Policy,
    local_policy: Policy,
    overlay_policy: Policy,
    max_stale_secs: i64,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: mirror_policy,
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            policy: local_policy,
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: token.then(|| "s3cret".to_owned()),
                volatile,
            },
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: overlay_policy,
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                upload: Some(1),
            },
        },
    ];
    let mut state = AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    );
    state.max_stale_secs = max_stale_secs;
    let state = crate::tests::wired(state);
    Harness {
        _dir: dir,
        server,
        state,
        clock,
    }
}

pub async fn harness() -> Harness {
    harness_with(true, true).await
}

pub async fn promotion_harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        },
        Index {
            name: "staging".to_owned(),
            route: "staging".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
            policy: Policy::default(),
        },
        Index {
            name: "prod".to_owned(),
            route: "prod".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
            policy: Policy::default(),
        },
        Index {
            name: "release".to_owned(),
            route: "release".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![2, 0],
                upload: Some(2),
            },
            policy: Policy::default(),
        },
    ];
    let state = crate::tests::wired(AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    ));
    Harness {
        _dir: dir,
        server,
        state,
        clock,
    }
}

pub fn policy(configure: impl FnOnce(&mut PolicyConfig, &mut PypiPolicyConfig)) -> Policy {
    let mut neutral = PolicyConfig::default();
    let mut pypi = PypiPolicyConfig::default();
    configure(&mut neutral, &mut pypi);
    Policy::compile(&neutral).with_rules(compile_rules(&pypi).unwrap())
}

pub fn put_raw_project_status(path: &Path, key: &str, value: &[u8]) {
    let db = redb::Database::create(path).unwrap();
    let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("project_status");
    let txn = db.begin_write().unwrap();
    txn.open_table(table).unwrap().insert(key, value).unwrap();
    txn.commit().unwrap();
}

pub async fn get(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let (status, headers, bytes) = get_bytes(state, uri, accept).await;
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

pub async fn get_with_headers(
    state: &Arc<AppState>,
    uri: &str,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(uri).method("GET");
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

pub async fn get_bytes(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().uri(uri).method("GET");
    if let Some(accept) = accept {
        builder = builder.header(header::ACCEPT, accept);
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

pub fn detail_json(digest: &str, file_url: &str) -> String {
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}"
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

/// Build a mirror harness whose cached flask page was fetched at `fetched_at`, and whose upstream
/// is unreachable, so the only question is whether the stale copy may still answer.
pub async fn stale_page_harness(max_stale_secs: i64, fetched_at: i64) -> Harness {
    let h = harness_with_stale(
        true,
        true,
        Policy::default(),
        Policy::default(),
        Policy::default(),
        max_stale_secs,
    )
    .await;
    let body = crate::to_json(&crate::ProjectDetail {
        meta: crate::Meta::default(),
        name: "flask".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![],
    });
    h.state
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: fetched_at,
                content_type: None,
                fresh_secs: None,
                body: body.into_bytes(),
            },
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    h
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
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, content_type);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::from(body)).unwrap())
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

#[tokio::test(flavor = "current_thread")]
pub async fn test_security_logs_upload_success_without_token_secret() {
    let h = harness().await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await,
        StatusCode::OK
    );

    drop(guard);
    let text = logs.text();
    assert!(!text.contains("s3cret"));
    let events = logs.security_events();
    assert!(events.iter().any(|event| {
        field(event, "action") == Some("token_use")
            && field(event, "result") == Some("success")
            && field(event, "actor") == Some("__token__")
            && field(event, "index") == Some("hosted")
    }));
    let upload = events
        .iter()
        .find(|event| field(event, "action") == Some("upload") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(upload, "index"), Some("root/pypi"));
    assert_eq!(field(upload, "hosted_index"), Some("hosted"));
    assert_eq!(field(upload, "project"), Some("peryxpkg"));
    assert_eq!(field(upload, "version"), Some("1.0"));
    assert_eq!(field(upload, "filename"), Some("peryxpkg-1.0-py3-none-any.whl"));
    assert_eq!(upload["fields"]["count"], 1);
    assert!(field(upload, "digest").is_some_and(|digest| digest.len() == 64));
}

#[tokio::test(flavor = "current_thread")]
pub async fn test_security_logs_invalid_token_without_secret() {
    let h = harness().await;
    let (content_type, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", b"x")));
    let auth = format!("Basic {}", STANDARD.encode("alice:nope"));
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&auth), &content_type, body).await,
        StatusCode::UNAUTHORIZED
    );

    drop(guard);
    let text = logs.text();
    assert!(!text.contains("nope"));
    assert!(!text.contains("s3cret"));
    let events = logs.security_events();
    let token = events
        .iter()
        .find(|event| field(event, "action") == Some("token_use") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(field(token, "actor"), Some("alice"));
    assert_eq!(field(token, "index"), Some("hosted"));
    assert_eq!(field(token, "reason"), Some("invalid upload token"));
}

#[tokio::test(flavor = "current_thread")]
pub async fn test_security_logs_delete_policy_denial() {
    let h = harness_with(true, false).await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::FORBIDDEN
    );

    drop(guard);
    let events = logs.security_events();
    let delete = events
        .iter()
        .find(|event| field(event, "action") == Some("delete") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(field(delete, "actor"), Some("__token__"));
    assert_eq!(field(delete, "index"), Some("hosted"));
    assert_eq!(field(delete, "hosted_index"), Some("hosted"));
    assert_eq!(field(delete, "project"), Some("peryxpkg"));
    assert_eq!(
        field(delete, "reason"),
        Some("index is not volatile; delete is disabled")
    );
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

pub fn fixture_wheel() -> Vec<u8> {
    fixture_wheel_for("1.0")
}

pub fn fixture_sdist() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content = b"Metadata-Version: 2.2\nName: peryxpkg\nVersion: 1.0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/PKG-INFO", content.as_slice())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/pyproject.toml", pyproject.as_slice())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}

pub fn fixture_sdist_without_pkg_info() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content = b"x = 1\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/module.py", content.as_slice())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/pyproject.toml", pyproject.as_slice())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}

pub fn fixture_wheel_for(version: &str) -> Vec<u8> {
    fixture_wheel_with_body(version, b"VALUE = 1\n")
}

pub fn fixture_wheel_with_body(version: &str, body: &[u8]) -> Vec<u8> {
    fixture_wheel_with_body_and_metadata(
        version,
        body,
        Some(format!("Metadata-Version: 2.1\nName: peryxpkg\nVersion: {version}\nRequires-Python: >=3.8\n").as_bytes()),
    )
}

pub fn fixture_wheel_without_metadata() -> Vec<u8> {
    fixture_wheel_with_body_and_metadata("1.0", b"VALUE = 1\n", None)
}

pub fn fixture_wheel_with_metadata(metadata: &[u8]) -> Vec<u8> {
    fixture_wheel_with_body_and_metadata("1.0", b"VALUE = 1\n", Some(metadata))
}

pub fn empty_zip() -> Vec<u8> {
    let mut bytes = Vec::new();
    zip::ZipWriter::new(std::io::Cursor::new(&mut bytes)).finish().unwrap();
    bytes
}

pub fn fixture_wheel_with_metadata_compression(metadata: &[u8], compression: zip::CompressionMethod) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(compression);
        let dist_info = "peryxpkg-1.0.dist-info";
        let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
        let entries = [
            ("peryxpkg/__init__.py".to_owned(), b"VALUE = 1\n".to_vec()),
            (format!("{dist_info}/METADATA"), metadata.to_vec()),
            (format!("{dist_info}/WHEEL"), wheel.to_vec()),
        ];
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

pub fn wheel_with_invalid_deflated_metadata(metadata: &[u8]) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let data_start = metadata_local_data_start(&wheel);
    wheel[data_start] = 0x07;
    wheel
}

pub fn wheel_with_metadata_compression_method(metadata: &[u8], compression_method: u16) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let position = metadata_central_directory_position(&wheel);
    wheel[position + 10..position + 12].copy_from_slice(&compression_method.to_le_bytes());
    wheel
}

pub fn wheel_with_metadata_uncompressed_size(metadata: &[u8], uncompressed_size: u32) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let position = metadata_central_directory_position(&wheel);
    wheel[position + 24..position + 28].copy_from_slice(&uncompressed_size.to_le_bytes());
    wheel
}

pub fn overwrite_metadata_local_signature(wheel: &mut [u8], signature: [u8; 4]) {
    let position = metadata_local_header_position(wheel);
    wheel[position..position + 4].copy_from_slice(&signature);
}

pub fn overwrite_metadata_central_signature(wheel: &mut [u8], signature: [u8; 4]) {
    let position = metadata_central_directory_position(wheel);
    wheel[position..position + 4].copy_from_slice(&signature);
}

pub fn metadata_local_data_start(wheel: &[u8]) -> usize {
    let position = metadata_local_header_position(wheel);
    let name_len = usize::from(u16::from_le_bytes(
        wheel[position + 26..position + 28].try_into().unwrap(),
    ));
    let extra_len = usize::from(u16::from_le_bytes(
        wheel[position + 28..position + 30].try_into().unwrap(),
    ));
    position + 30 + name_len + extra_len
}

pub fn metadata_local_header_position(wheel: &[u8]) -> usize {
    let metadata = b"peryxpkg-1.0.dist-info/METADATA";
    for position in 0..wheel.len().saturating_sub(30) {
        if !wheel[position..].starts_with(b"PK\x03\x04") {
            continue;
        }
        let name_len = usize::from(u16::from_le_bytes(
            wheel[position + 26..position + 28].try_into().unwrap(),
        ));
        let name_start = position + 30;
        let name_end = name_start + name_len;
        if wheel.get(name_start..name_end) == Some(metadata.as_slice()) {
            return position;
        }
    }
    panic!("metadata local header not found");
}

pub fn metadata_central_directory_position(wheel: &[u8]) -> usize {
    let metadata = b"peryxpkg-1.0.dist-info/METADATA";
    for position in 0..wheel.len().saturating_sub(46) {
        if !wheel[position..].starts_with(b"PK\x01\x02") {
            continue;
        }
        let name_len = usize::from(u16::from_le_bytes(
            wheel[position + 28..position + 30].try_into().unwrap(),
        ));
        let name_start = position + 46;
        let name_end = name_start + name_len;
        if wheel.get(name_start..name_end) == Some(metadata.as_slice()) {
            return position;
        }
    }
    panic!("metadata central directory entry not found");
}

pub fn fixture_wheel_with_body_and_metadata(version: &str, body: &[u8], metadata: Option<&[u8]>) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let dist_info = format!("peryxpkg-{version}.dist-info");
        let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
        let mut entries = vec![("peryxpkg/__init__.py".to_owned(), body.to_vec())];
        if let Some(metadata) = metadata {
            entries.push((format!("{dist_info}/METADATA"), metadata.to_vec()));
        }
        entries.push((format!("{dist_info}/WHEEL"), wheel.to_vec()));
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

pub fn record(entries: &[(String, Vec<u8>)], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
        writeln!(record, "{path},sha256={digest},{}", bytes.len()).unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

pub async fn upload_wheel(state: &Arc<AppState>, filename: &str, bytes: &[u8]) -> Digest {
    upload_wheel_to(state, "/hosted/", filename, "1.0", bytes).await
}

pub async fn upload_wheel_to(state: &Arc<AppState>, uri: &str, filename: &str, version: &str, bytes: &[u8]) -> Digest {
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", version),
        ("filetype", "bdist_wheel"),
    ];
    let (ct, body) = multipart_body(&fields, Some((filename, bytes)));
    assert_eq!(
        post_upload(state, uri, Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
    Digest::of(bytes)
}

pub fn blob_count(state: &AppState) -> u64 {
    let mut count = 0;
    state
        .blobs
        .scan(|_entry| {
            count += 1;
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    count
}

pub fn upload_record(
    filename: &str,
    version: &str,
    url: String,
    hashes: BTreeMap<String, String>,
    size: Option<u64>,
) -> Uploaded {
    Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.to_owned(),
            url,
            hashes,
            requires_python: None,
            size,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    }
}

pub fn put_local_file(state: &AppState, filename: &str, bytes: &[u8], version: &str) -> Digest {
    let digest = Digest::of(bytes);
    state.blobs.write_verified(bytes, &digest).unwrap();
    let uploaded = upload_record(
        filename,
        version,
        local_file_url("hosted", digest.as_str(), filename),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(bytes.len() as u64),
    );
    state
        .meta
        .put_upload("hosted", "peryxpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    state.meta.put_project("hosted", "peryxpkg", "peryxpkg").unwrap();
    digest
}
