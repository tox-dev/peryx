use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_core::{BrowseLink, BrowsePage, BrowseSection, Ecosystem};
use peryx_driver::AppState;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use rstest::rstest;

use super::browse_http;
use crate::store::{CachedIndex, PypiStore as _};

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
    let digest = peryx_storage::blob::Digest::of(b"remote wheel");
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
    let digest = peryx_storage::blob::Digest::of(b"remote wheel");
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
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0-py3-none-any.whl","url":"https://files.example/flask.whl","hashes":{{"sha256":"{digest}"}}}}]}}"#
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
