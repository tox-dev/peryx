use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_core::path::local_artifact_url;
use peryx_core::{BrowseLink, BrowsePage, BrowseSection, Ecosystem};
use peryx_driver::AppState;
use peryx_identity::{
    Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, Role, SESSION_COOKIE, ServerUser, SessionSealer,
};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use rstest::rstest;

use super::browse_http;

const SESSION_KEY: &[u8] = b"a-token-realm-signing-secret-here";
use crate::store::{CachedIndex, PypiStore as _};
use crate::upload::Uploaded;
use crate::{CoreMetadata, File, Provenance, Yanked};

#[tokio::test]
async fn upload_form_lists_escaped_writable_pypi_indexes() {
    let (_directory, state) = app(vec![
        index(
            "hosted<&>\"'",
            "route<&>\"'",
            crate::ECOSYSTEM,
            IndexKind::Hosted { volatile: false },
            writer_acl(),
        ),
        index(
            "cached",
            "cached",
            crate::ECOSYSTEM,
            IndexKind::Cached {
                client: UpstreamClient::new("https://example.invalid/simple/").unwrap(),
                offline: true,
            },
            IndexAcl::default(),
        ),
        index(
            "foreign",
            "foreign",
            Ecosystem::new("foreign"),
            IndexKind::Hosted { volatile: false },
            IndexAcl::default(),
        ),
    ]);

    let (status, headers, body) = send(state, Method::GET, "/upload", None).await;

    assert_eq!(
        (
            status,
            headers.get(header::CACHE_CONTROL),
            body.matches("<option").count()
        ),
        (StatusCode::OK, Some(&"no-store".parse().unwrap()), 1)
    );
    assert!(body.contains(r#"<option value="/route&lt;&amp;&gt;&quot;&#39;/">hosted&lt;&amp;&gt;&quot;&#39; (route&lt;&amp;&gt;&quot;&#39;)</option>"#));
}

#[tokio::test]
async fn upload_form_rejects_non_get_requests() {
    let (_directory, state) = app(vec![]);

    let (status, _, body) = send(state, Method::POST, "/upload", None).await;

    assert_eq!((status, body), (StatusCode::METHOD_NOT_ALLOWED, String::new()));
}

#[tokio::test]
async fn browse_http_returns_a_no_store_project_list() {
    let (_directory, state) = app(vec![hosted(IndexAcl::default())]);

    let (status, headers, body) = send(state, Method::GET, "/browse?index=hosted", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        serde_json::from_str::<BrowsePage>(&body).unwrap(),
        BrowsePage {
            title: "hosted".to_owned(),
            sections: vec![BrowseSection::Links {
                heading: "Projects".to_owned(),
                entries: Vec::<BrowseLink>::new(),
                empty: "No projects observed on this index yet.".to_owned(),
            }],
            ..BrowsePage::default()
        }
    );
}

#[tokio::test]
async fn browse_http_distinguishes_bad_queries_and_missing_resources() {
    for (uri, expected_status, expected_body) in [
        ("/browse", StatusCode::BAD_REQUEST, "missing index"),
        (
            "/browse?index=hosted&offset=soon",
            StatusCode::BAD_REQUEST,
            "invalid archive offset \"soon\"",
        ),
        ("/browse?index=missing", StatusCode::NOT_FOUND, ""),
        ("/browse?index=hosted&project=ghost", StatusCode::NOT_FOUND, ""),
    ] {
        let (_directory, state) = app(vec![hosted(IndexAcl::default())]);
        let (status, _, body) = send(state, Method::GET, uri, None).await;

        assert_eq!((status, body.as_str()), (expected_status, expected_body), "{uri}");
    }
}

#[rstest]
#[case::utf8(&[0xff], "is not UTF-8")]
#[case::source(b"https://files.example/flask.whl", "missing field \"source\"")]
#[case::size(b"https://files.example/flask.whl\npypi\nlarge", "invalid integer field \"size\"")]
#[tokio::test]
async fn browse_http_reports_a_source_record_decode_failure(#[case] record: &[u8], #[case] expected: &str) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let digest = Digest::of(b"remote wheel");
    seed_cached_project(&meta, digest.as_str());
    meta.put_driver_value(&format!("pypi\0f\0{}", digest.as_str()), record)
        .unwrap();

    let state = cached_app(&directory, meta);
    let (status, _, body) = send(state, Method::GET, "/browse?index=pypi&project=flask", None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains(expected), "{body}");
}

#[tokio::test]
async fn browse_http_reports_a_placement_record_read_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let digest = Digest::of(b"remote wheel");
    seed_cached_project(&meta, digest.as_str());
    drop(meta);
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    write
        .open_table(redb::TableDefinition::<&str, &str>::new("artifact_placement"))
        .unwrap();
    write.commit().unwrap();
    drop(database);

    let state = cached_app(&directory, MetaStore::open_existing(&path).unwrap());
    let (status, _, body) = send(state, Method::GET, "/browse?index=pypi&project=flask", None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("artifact_placement is of type"), "{body}");
}

#[tokio::test]
async fn browse_http_reports_archive_query_errors() {
    for (uri, expected_body) in [
        (
            "/browse?index=hosted&project=ghost&sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "archive query requires file",
        ),
        (
            "/browse?index=hosted&project=ghost&member=README.txt",
            "archive query requires file and sha256",
        ),
        (
            "/browse?index=hosted&project=ghost&file=bad%2Fdemo.whl",
            "artifact on index \"hosted\" for file \"demo.whl\": invalid sha256 digest \"bad\"",
        ),
        (
            "/browse?index=hosted&project=ghost&file=demo.whl",
            "archive query requires sha256",
        ),
    ] {
        let (_directory, state) = app(vec![hosted(IndexAcl::default())]);
        let (status, _, body) = send(state, Method::GET, uri, None).await;

        assert_eq!(
            (status, body.as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, expected_body),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn browse_http_challenges_an_anonymous_private_reader() {
    let (_directory, state) = app(vec![hosted(reader_acl("*"))]);

    let (status, headers, body) = send(state, Method::GET, "/browse?index=hosted", None).await;

    assert_eq!(
        (status, headers.get(header::WWW_AUTHENTICATE), body.as_str()),
        (
            StatusCode::UNAUTHORIZED,
            Some(&"Basic realm=\"peryx\"".parse().unwrap()),
            "unauthorized"
        )
    );
}

#[tokio::test]
async fn browse_http_authorizes_a_signed_in_repository_reader() {
    let (_directory, state, reader, stranger) = browser_app(vec![hosted(reader_acl("other"))]);

    let allowed = send_as_signed_in(state.clone(), "/browse?index=hosted", &reader).await;
    let denied = send_as_signed_in(state, "/browse?index=hosted", &stranger).await;

    assert_eq!((allowed.0, denied.0), (StatusCode::OK, StatusCode::FORBIDDEN));
    assert!(allowed.2.contains("hosted"), "{}", allowed.2);
}

#[tokio::test]
async fn browse_http_forbids_a_reader_from_an_ungranted_project() {
    let (_directory, state) = app(vec![hosted(reader_acl("other"))]);
    let authorization = format!("Basic {}", STANDARD.encode("reader:secret"));

    let (status, _, body) = send(
        state,
        Method::GET,
        "/browse?index=hosted&project=demo",
        Some(&authorization),
    )
    .await;

    assert_eq!((status, body), (StatusCode::FORBIDDEN, String::new()));
}

#[tokio::test]
async fn browse_http_reads_either_spelling_of_a_project_under_a_normalized_grant() {
    let (_directory, state, _digest, authorization) = display_named_app();

    let display = send(
        state.clone(),
        Method::GET,
        "/browse?index=hosted&project=Flask",
        Some(&authorization),
    )
    .await;
    let normalized = send(
        state.clone(),
        Method::GET,
        "/browse?index=hosted&project=flask",
        Some(&authorization),
    )
    .await;

    assert_eq!(
        (display.0, normalized.0, display.2.as_str()),
        (StatusCode::OK, StatusCode::OK, normalized.2.as_str())
    );
}

#[tokio::test]
async fn browse_http_forbids_an_ungranted_project_that_normalizes_elsewhere() {
    let (_directory, state, _digest, authorization) = display_named_app();

    let (status, _, body) = send(
        state,
        Method::GET,
        "/browse?index=hosted&project=Other",
        Some(&authorization),
    )
    .await;

    assert_eq!((status, body), (StatusCode::FORBIDDEN, String::new()));
}

#[tokio::test]
async fn browse_http_authorizes_the_project_link_the_index_listing_renders() {
    let (_directory, state, _digest, authorization) = display_named_app();

    let (_, _, listing) = send(state.clone(), Method::GET, "/browse?index=hosted", Some(&authorization)).await;
    let (status, _, body) = send(
        state,
        Method::GET,
        "/browse?index=hosted&project=Flask",
        Some(&authorization),
    )
    .await;

    assert_eq!(
        serde_json::from_str::<BrowsePage>(&listing).unwrap().sections,
        vec![BrowseSection::Links {
            heading: "Projects".to_owned(),
            entries: vec![BrowseLink {
                label: "Flask".to_owned(),
                href: "/browse?index=hosted&project=Flask".to_owned(),
            }],
            empty: "No projects observed on this index yet.".to_owned(),
        }]
    );
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn browse_http_authorizes_the_archive_link_a_file_row_renders() {
    let (_directory, state, digest, authorization) = display_named_app();
    let archive = format!("/browse?index=hosted&project=Flask&sha256={digest}&file={WHEEL}");

    let (_, _, project) = send(
        state.clone(),
        Method::GET,
        "/browse?index=hosted&project=Flask",
        Some(&authorization),
    )
    .await;
    let (status, _, body) = send(state, Method::GET, &archive, Some(&authorization)).await;

    assert!(project.contains(&archive), "{project}");
    assert_eq!(
        (status, serde_json::from_str::<BrowsePage>(&body).unwrap().title),
        (StatusCode::OK, WHEEL.to_owned())
    );
}

const WHEEL: &str = "flask-1.0-py3-none-any.whl";

/// A hosted index holding one project stored as `Flask` and normalized to `flask`, read by a token
/// whose grant names only the normalized spelling. Returns the wheel's digest and that token's
/// `Authorization` header.
fn display_named_app() -> (tempfile::TempDir, Arc<AppState>, String, String) {
    let (directory, state) = app(vec![hosted(reader_acl("flask"))]);
    let mut bytes = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        archive
            .start_file("README.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"read me\n").unwrap();
        archive.finish().unwrap();
    }
    let digest = Digest::of(&bytes);
    state.serving.blobs.blocking().put_bytes_as(&bytes, &digest).unwrap();
    let record = Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: WHEEL.to_owned(),
            url: local_artifact_url("hosted", digest.as_str(), WHEEL),
            hashes: BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(bytes.len() as u64),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: None,
    };
    state
        .serving
        .meta
        .put_upload("hosted", "flask", WHEEL, &serde_json::to_vec(&record).unwrap())
        .unwrap();
    state.serving.meta.put_project("hosted", "flask", "Flask").unwrap();
    let authorization = format!("Basic {}", STANDARD.encode("reader:secret"));
    (directory, state, digest.as_str().to_owned(), authorization)
}

async fn send(
    state: Arc<AppState>,
    method: Method,
    uri: &str,
    authorization: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().method(method).uri(uri).body(Body::empty()).unwrap();
    if let Some(authorization) = authorization {
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, authorization.parse().unwrap());
    }
    dispatch(state, request).await
}

async fn send_as_signed_in(state: Arc<AppState>, uri: &str, cookie: &str) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    request.headers_mut().insert(header::COOKIE, cookie.parse().unwrap());
    dispatch(state, request).await
}

async fn dispatch(state: Arc<AppState>, request: Request) -> (StatusCode, HeaderMap, String) {
    let response = browse_http(state, request).await;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn app(indexes: Vec<Index>) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        indexes,
    );
    crate::tests::install(&mut state);
    (directory, Arc::new(state))
}

/// Builds a state with browser sessions enabled, returning the cookies of a user granted
/// `repository_reader` on `hosted` and of a signed-in user holding no grant at all.
fn browser_app(indexes: Vec<Index>) -> (tempfile::TempDir, Arc<AppState>, String, String) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        indexes,
    );
    crate::tests::install(&mut state);
    state.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    let reader = state.serving.users.create("Rita").unwrap();
    state
        .serving
        .authorization
        .grant(
            &reader.id,
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "hosted".to_owned(),
            },
        )
        .unwrap();
    let stranger = state.serving.users.create("Sam").unwrap();
    let cookies = (session_cookie(&reader), session_cookie(&stranger));
    (directory, Arc::new(state), cookies.0, cookies.1)
}

fn session_cookie(user: &ServerUser) -> String {
    format!(
        "{SESSION_COOKIE}={}",
        SessionSealer::new(SESSION_KEY).seal_session(user, 4_102_444_800)
    )
}

fn cached_app(directory: &tempfile::TempDir, meta: MetaStore) -> Arc<AppState> {
    let mut state = AppState::new(
        meta,
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        vec![index(
            "pypi",
            "pypi",
            crate::ECOSYSTEM,
            IndexKind::Cached {
                client: UpstreamClient::new("https://example.invalid/simple/").unwrap(),
                offline: true,
            },
            IndexAcl::default(),
        )],
    );
    crate::tests::install(&mut state);
    Arc::new(state)
}

fn seed_cached_project(meta: &MetaStore, digest: &str) {
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0-py3-none-any.whl","size":11,"url":"https://files.example/flask.whl","hashes":{{"sha256":"{digest}"}}}}]}}"#
    );
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 900,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: None,
            body: body.into_bytes(),
        },
    )
    .unwrap();
}

fn hosted(acl: IndexAcl) -> Index {
    index(
        "hosted",
        "hosted",
        crate::ECOSYSTEM,
        IndexKind::Hosted { volatile: false },
        acl,
    )
}

fn index(name: &str, route: &str, ecosystem: Ecosystem, kind: IndexKind, acl: IndexAcl) -> Index {
    Index {
        name: name.to_owned(),
        route: route.to_owned(),
        ecosystem,
        kind,
        policy: Policy::default(),
        acl,
    }
}

fn reader_acl(resource: &str) -> IndexAcl {
    IndexAcl {
        anonymous_read: false,
        tokens: vec![NamedToken {
            name: "reader".to_owned(),
            secret: "secret".to_owned(),
            grants: vec![Grant {
                resources: vec![Glob::new(resource)],
                actions: BTreeSet::from([Action::Read]),
            }],
            expires_at: None,
        }],
    }
}

fn writer_acl() -> IndexAcl {
    IndexAcl {
        anonymous_read: false,
        tokens: vec![NamedToken {
            name: "writer".to_owned(),
            secret: "secret".to_owned(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write]),
            }],
            expires_at: None,
        }],
    }
}
