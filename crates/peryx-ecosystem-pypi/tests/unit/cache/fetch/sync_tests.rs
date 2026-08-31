use std::collections::BTreeMap;

use peryx_index::serving::Inflight;
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    MAX_PROJECT_BYTES, PROJECT_FILE_BATCH, ParseProject, ProjectSyncError, ProjectSyncOutcome, parse_project,
    publish_project_response, sync_project_files, write_project_chunk,
};
use crate::SimpleClientExt as _;
use crate::simple::{CoreMetadata, File, Provenance, Yanked};
use crate::store::PypiStore as _;
use crate::store::{
    FilePublication, MetadataClaim, ProjectGeneration, active_project_generation, begin_project_generation,
    get_file_publication, list_project_files, project_meta_state, publish_project_generation, put_project_files,
};

const JSON: &str = "application/vnd.pypi.simple.v1+json";

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn file(filename: &str, sha256: &str) -> File {
    File {
        filename: filename.to_owned(),
        url: format!("https://files.example/{filename}"),
        hashes: BTreeMap::from([("sha256".to_owned(), sha256.to_owned())]),
        requires_python: Some(">=3.8".to_owned()),
        size: Some(10),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
}

fn seed_active(meta: &MetaStore, index: &str, project: &str, etag: &str, files: &[File]) -> u64 {
    let (id, expected) = begin_project_generation(meta, index, project).unwrap();
    let admitted = put_project_files(meta, index, project, id, index, None, files).unwrap();
    publish_project_generation(
        meta,
        index,
        project,
        expected,
        ProjectGeneration {
            generation: id,
            source: index.to_owned(),
            url: "https://pypi.org/simple/flask/".to_owned(),
            format: "json".to_owned(),
            etag: Some(etag.to_owned()),
            last_modified: None,
            last_serial: None,
            fetched_at_unix: 1,
            bytes: 1,
            files: admitted,
            versions: Vec::new(),
            project_status: None,
            project_status_reason: None,
        },
    )
    .unwrap();
    id
}

fn client_for(server: &MockServer) -> UpstreamClient {
    UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap()
}

#[tokio::test]
async fn test_sync_publishes_a_json_detail() {
    let server = MockServer::start().await;
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[
            {{"filename":"flask-1.0-py3-none-any.whl","url":"flask-1.0-py3-none-any.whl","hashes":{{"sha256":"{a}"}},"size":10}},
            {{"filename":"flask-1.0.tar.gz","url":"flask-1.0.tar.gz","hashes":{{"sha256":"{b}"}}}}]}}"#,
        a = "a".repeat(64),
        b = "b".repeat(64),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "v1")
                .set_body_raw(body, JSON),
        )
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    let outcome = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert_eq!(outcome, ProjectSyncOutcome::Published { files: 2 });
    let files = list_project_files(&meta, "pypi", "flask").unwrap();
    assert_eq!(files.len(), 2);
    let active = active_project_generation(&meta, "pypi", "flask").unwrap().unwrap();
    assert_eq!(active.format, "json");
    assert_eq!(active.etag.as_deref(), Some("v1"));
    assert!(meta.get_file_url(&"a".repeat(64)).unwrap().is_some());
}

#[tokio::test]
async fn test_sync_html_and_json_agree_on_shared_fields() {
    let sha = "a".repeat(64);
    let json = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
            {{"filename":"flask-1.0.tar.gz","url":"https://files.example/flask-1.0.tar.gz","hashes":{{"sha256":"{sha}"}},"requires-python":">=3.8","size":10}}]}}"#,
    );
    let html = format!(
        r#"<!DOCTYPE html><html><body><a href="https://files.example/flask-1.0.tar.gz#sha256={sha}" data-requires-python="&gt;=3.8" data-size="10">flask-1.0.tar.gz</a></body></html>"#,
    );

    let mut listed = Vec::new();
    for (format, body, media) in [
        ("json", json, JSON),
        ("html", html, "application/vnd.pypi.simple.v1+html"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, media))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();
        sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            format,
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap();
        listed.push(list_project_files(&meta, format, "flask").unwrap());
    }

    let (json_files, html_files) = (&listed[0], &listed[1]);
    assert_eq!(json_files.len(), 1);
    assert_eq!(json_files[0].filename, html_files[0].filename);
    assert_eq!(json_files[0].url, html_files[0].url);
    assert_eq!(json_files[0].hashes, html_files[0].hashes);
    assert_eq!(json_files[0].size, html_files[0].size);
    assert_eq!(json_files[0].requires_python, html_files[0].requires_python);
}

#[tokio::test]
async fn test_sync_304_reuses_the_active_generation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("if-none-match", "v1"))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "v2"))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();
    let id = seed_active(
        &meta,
        "pypi",
        "flask",
        "v1",
        &[file("flask-1.0.tar.gz", &"a".repeat(64))],
    );

    let outcome = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert_eq!(outcome, ProjectSyncOutcome::NotModified { files: 1 });
    let active = active_project_generation(&meta, "pypi", "flask").unwrap().unwrap();
    assert_eq!(
        active.generation, id,
        "a 304 keeps the same generation, so artifact placement is untouched"
    );
    assert_eq!(active.etag.as_deref(), Some("v2"));
    assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap().len(), 1);
}

#[tokio::test]
async fn test_sync_304_without_an_active_generation_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    let error = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ProjectSyncError::Store(_)));
}

#[tokio::test]
async fn test_sync_404_leaves_the_prior_generation_serviceable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();
    seed_active(
        &meta,
        "pypi",
        "flask",
        "v1",
        &[file("flask-1.0.tar.gz", &"a".repeat(64))],
    );

    let outcome = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert_eq!(outcome, ProjectSyncOutcome::Missing);
    assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap().len(), 1);
}

#[tokio::test]
async fn test_sync_incomplete_detail_preserves_the_active_generation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"meta":{"api-version":"1.0"},"name":"flask"}"#, JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();
    let id = seed_active(
        &meta,
        "pypi",
        "flask",
        "v1",
        &[file("flask-1.0.tar.gz", &"a".repeat(64))],
    );

    let error = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ProjectSyncError::Simple(_)));
    let state = project_meta_state(&meta, "pypi", "flask").unwrap();
    assert_eq!(state.active.unwrap().generation, id);
    assert!(state.staging.is_none());
    assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap().len(), 1);
}

#[tokio::test]
async fn test_sync_replaces_the_active_generation_and_sweeps_the_retired_one() {
    let server = MockServer::start().await;
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
            {{"filename":"flask-2.0.tar.gz","url":"flask-2.0.tar.gz","hashes":{{"sha256":"{b}"}}}},
            {{"filename":"flask-2.0-py3-none-any.whl","url":"flask-2.0-py3-none-any.whl","hashes":{{"sha256":"{c}"}}}}]}}"#,
        b = "b".repeat(64),
        c = "c".repeat(64),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();
    seed_active(
        &meta,
        "pypi",
        "flask",
        "v1",
        &[file("flask-1.0.tar.gz", &"a".repeat(64))],
    );

    let outcome = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert_eq!(outcome, ProjectSyncOutcome::Published { files: 2 });
    let files = list_project_files(&meta, "pypi", "flask").unwrap();
    assert_eq!(files.len(), 2, "only the new generation's files remain servable");
    let state = project_meta_state(&meta, "pypi", "flask").unwrap();
    assert!(
        state.retired.is_none(),
        "the displaced generation is swept after publication"
    );
}

#[tokio::test]
async fn test_sync_skips_a_file_without_a_hash() {
    let server = MockServer::start().await;
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
            {{"filename":"flask-1.0.tar.gz","url":"flask-1.0.tar.gz","hashes":{{"sha256":"{a}"}}}},
            {{"filename":"unhashed.tar.gz","url":"unhashed.tar.gz","hashes":{{}}}}]}}"#,
        a = "a".repeat(64),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    let outcome = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert_eq!(outcome, ProjectSyncOutcome::Published { files: 1 });
}

#[tokio::test]
async fn test_sync_registers_upstream_provenance_with_the_cached_index() {
    let server = MockServer::start().await;
    let digest = "a".repeat(64);
    let filename = "flask-1.0.tar.gz";
    let provenance = format!("{}/integrity/{filename}.provenance", server.uri());
    let body = format!(
        r#"{{"meta":{{"api-version":"1.4"}},"name":"flask","files":[{{"filename":"{filename}","url":"{filename}","hashes":{{"sha256":"{digest}"}},"provenance":"{provenance}"}}]}}"#,
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    let record = meta
        .get_upstream_attestation("pypi", "flask", &digest, filename)
        .unwrap()
        .unwrap();
    assert_eq!(record.url, provenance);
    assert_eq!(record.source, "pypi");
    assert_eq!(record.upstream, None);
}

#[tokio::test]
async fn test_sync_returns_the_upstream_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    let error = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ProjectSyncError::Status(500)));
}

#[tokio::test]
async fn test_sync_coalesces_concurrent_fetches() {
    let server = MockServer::start().await;
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
            {{"filename":"flask-1.0.tar.gz","url":"flask-1.0.tar.gz","hashes":{{"sha256":"{a}"}}}}]}}"#,
        a = "a".repeat(64),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();
    let inflight = Inflight::default();
    let policy = Policy::default();

    let (first, second) = tokio::join!(
        sync_project_files(&client, &inflight, &meta, "pypi", &policy, "flask", client.base_url()),
        sync_project_files(&client, &inflight, &meta, "pypi", &policy, "flask", client.base_url()),
    );

    assert_eq!(first.unwrap(), ProjectSyncOutcome::Published { files: 1 });
    assert_eq!(second.unwrap(), ProjectSyncOutcome::NotModified { files: 1 });
}

#[tokio::test]
async fn test_sync_scopes_same_key_coalescing_to_one_store() {
    let first_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("if-none-match", "v1"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&first_server)
        .await;
    let second_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("if-none-match", "v1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&second_server)
        .await;
    let first_client = client_for(&first_server);
    let second_client = client_for(&second_server);
    let (_first_dir, first_meta) = store();
    let (_second_dir, second_meta) = store();
    let previous = [file("flask-1.0.tar.gz", &"a".repeat(64))];
    seed_active(&first_meta, "pypi", "flask", "v1", &previous);
    seed_active(&second_meta, "pypi", "flask", "v1", &previous);
    let first_inflight = Inflight::default();
    let second_inflight = Inflight::default();
    let policy = Policy::default();

    let (first, second) = tokio::join!(
        sync_project_files(
            &first_client,
            &first_inflight,
            &first_meta,
            "pypi",
            &policy,
            "flask",
            first_client.base_url(),
        ),
        sync_project_files(
            &second_client,
            &second_inflight,
            &second_meta,
            "pypi",
            &policy,
            "flask",
            second_client.base_url(),
        ),
    );

    assert_eq!(first.unwrap(), ProjectSyncOutcome::NotModified { files: 1 });
    assert_eq!(second.unwrap(), ProjectSyncOutcome::Missing);
}

#[tokio::test]
async fn test_sync_rejects_a_declared_oversize_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();
    let mut head = client.head_project("flask", None).await.unwrap();
    head.content_length = Some(MAX_PROJECT_BYTES + 1);

    let error = publish_project_response(&meta, "pypi", &Policy::default(), "flask", client.base_url(), head, 1)
        .await
        .unwrap_err();

    assert!(matches!(error, ProjectSyncError::TooLarge));
    assert!(active_project_generation(&meta, "pypi", "flask").unwrap().is_none());
}

#[tokio::test]
async fn test_sync_reports_an_unreachable_upstream() {
    let client = UpstreamClient::new("http://127.0.0.1:1/simple/").unwrap();
    let (_dir, meta) = store();

    let error = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ProjectSyncError::Upstream(_)));
}

#[test]
fn test_write_project_chunk_caps_unknown_length() {
    let mut output = Vec::new();
    let mut bytes = 0;
    write_project_chunk(&mut output, b"1234", &mut bytes, 7).unwrap();
    let error = write_project_chunk(&mut output, b"5678", &mut bytes, 7).unwrap_err();
    assert!(matches!(error, ProjectSyncError::TooLarge));
    assert_eq!(output, b"1234");
}

#[test]
fn test_write_project_chunk_propagates_a_writer_error() {
    struct FailWriter;
    impl std::io::Write for FailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
    }
    let mut writer = FailWriter;
    let mut bytes = 0;
    let error = write_project_chunk(&mut writer, b"data", &mut bytes, 100).unwrap_err();
    assert!(matches!(error, ProjectSyncError::Io(_)));
    assert!(
        std::io::Write::flush(&mut writer).is_err(),
        "the doubled writer fails every operation"
    );
}

#[test]
fn test_project_sync_error_messages_name_the_limit() {
    assert_eq!(
        ProjectSyncError::Status(500).to_string(),
        "upstream project detail returned 500"
    );
    assert!(ProjectSyncError::TooLarge.to_string().contains("byte limit"));
    assert!(ProjectSyncError::TooManyFiles.to_string().contains("file limit"));
    assert_eq!(ProjectSyncError::Io(std::io::Error::other("boom")).to_string(), "boom");
}

#[test]
fn test_parse_project_rejects_too_many_files() {
    let (_dir, meta) = store();
    let (id, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
    let html = format!(
        r#"<a href="https://files.example/a.tar.gz#sha256={a}">a.tar.gz</a><a href="https://files.example/b.tar.gz#sha256={b}">b.tar.gz</a>"#,
        a = "a".repeat(64),
        b = "b".repeat(64),
    );

    let error = parse_project(
        &mut std::io::Cursor::new(html),
        ParseProject {
            format: "html",
            base: &url::Url::parse("https://files.example/simple/flask/").unwrap(),
            meta: &meta,
            index: "pypi",
            policy: &Policy::default(),
            project: "flask",
            generation: id,
            upstream: None,
            max_files: 1,
        },
    )
    .unwrap_err();

    assert!(matches!(error, ProjectSyncError::TooManyFiles));
}

#[test]
fn test_parse_project_flushes_at_the_batch_limit() {
    let (_dir, meta) = store();
    let (id, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
    let files = (0..PROJECT_FILE_BATCH)
        .map(|index| {
            format!(
                r#"{{"filename":"pkg-{index}.tar.gz","url":"pkg-{index}.tar.gz","hashes":{{"sha256":"{index:064}"}}}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[{files}]}}"#);

    let (admitted, _) = parse_project(
        &mut std::io::Cursor::new(body),
        ParseProject {
            format: "json",
            base: &url::Url::parse("https://files.example/simple/flask/").unwrap(),
            meta: &meta,
            index: "pypi",
            policy: &Policy::default(),
            project: "flask",
            generation: id,
            upstream: None,
            max_files: super::MAX_PROJECT_FILES,
        },
    )
    .unwrap();

    assert_eq!(admitted, PROJECT_FILE_BATCH as u64);
}

#[tokio::test]
async fn test_sync_folds_an_upper_case_html_digest_into_the_stored_file_row() {
    let sha = "a1b2".repeat(16);
    let body = format!(
        r#"<!DOCTYPE html><html><body><a href="https://files.example/flask-1.0.tar.gz#sha256={}">flask-1.0.tar.gz</a></body></html>"#,
        sha.to_ascii_uppercase()
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/vnd.pypi.simple.v1+html"))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert!(meta.get_file_url(&sha).unwrap().is_some());
}

#[tokio::test]
async fn test_sync_drops_a_file_whose_digest_cannot_content_address() {
    let body = r#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],"files":[
        {"filename":"flask-1.0-py3-none-any.whl","url":"https://files.example/flask-1.0-py3-none-any.whl",
         "hashes":{"sha256":"not-a-digest"}}]}"#;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    let outcome = sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert_eq!(
        (outcome, list_project_files(&meta, "pypi", "flask").unwrap()),
        (ProjectSyncOutcome::Published { files: 0 }, Vec::new())
    );
}

#[rstest::rstest]
#[case::claimed(
    "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35",
    Some(FilePublication::Claimed(MetadataClaim {
        url: "https://files.example/flask-1.0-py3-none-any.whl.metadata".to_owned(),
        metadata_sha256: "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35".to_owned(),
        source: "pypi".to_owned(),
        upstream: None,
    }))
)]
#[case::delimiter(
    "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab\nu",
    Some(FilePublication::Unclaimed)
)]
#[tokio::test]
async fn test_sync_stores_a_sidecar_claim_only_for_a_digest_that_content_addresses(
    #[case] advertised: &str,
    #[case] expected: Option<FilePublication>,
) {
    let sha = "a1b2".repeat(16);
    let body = serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": ["1.0"],
        "files": [{
            "filename": "flask-1.0-py3-none-any.whl",
            "url": "https://files.example/flask-1.0-py3-none-any.whl",
            "hashes": {"sha256": sha},
            "core-metadata": {"sha256": advertised},
        }],
    })
    .to_string();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let (_dir, meta) = store();

    sync_project_files(
        &client,
        &Inflight::default(),
        &meta,
        "pypi",
        &Policy::default(),
        "flask",
        client.base_url(),
    )
    .await
    .unwrap();

    assert_eq!(
        get_file_publication(&meta, "pypi", "flask", &sha, "flask-1.0-py3-none-any.whl").unwrap(),
        expected
    );
}
