use std::cell::RefCell;
use std::io::{Cursor, Write as _};
use std::rc::Rc;

use flate2::Compression;
use flate2::write::GzEncoder;
use peryx_index::serving::Inflight;
use peryx_upstream::UpstreamClient;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    CatalogBatcher, CatalogSyncError, CatalogSyncOutcome, GenerationSink, HtmlSink, HtmlState, HtmlTokenizer,
    MAX_CATALOG_BYTES, MAX_CATALOG_PROJECTS, parse_catalog_with_limit, publish_response, read_catalog_projects,
    redact_url, sync_catalog, write_catalog_chunk, write_catalog_stream,
};
use crate::SimpleClientExt as _;
use crate::simple_client::CachedValidators;
use crate::store::{
    CatalogGeneration, abort_catalog_generation, begin_catalog_generation, catalog_state, list_projects,
    publish_catalog_generation, put_catalog_projects,
};
use peryx_storage::meta::MetaStore;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn active(generation: u64) -> CatalogGeneration {
    CatalogGeneration {
        generation,
        source: "test".to_owned(),
        url: "https://example.invalid/simple/".to_owned(),
        format: "json".to_owned(),
        etag: Some("old".to_owned()),
        last_modified: Some("yesterday".to_owned()),
        last_serial: None,
        fetched_at_unix: 1,
        bytes: 1,
        projects: 1,
    }
}

fn seed_active(meta: &MetaStore, index: &str) -> u64 {
    let (generation, expected) = begin_catalog_generation(meta, index).unwrap();
    publish_catalog_generation(meta, index, expected, active(generation)).unwrap();
    generation
}

#[tokio::test]
async fn test_sync_catalog_rejects_304_without_active_generation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, meta) = store();

    let error = sync_catalog(&client, &Inflight::default(), &meta, "no-active-304", client.base_url())
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogSyncError::Store(_)));
    assert!(catalog_state(&meta, "no-active-304").unwrap().active.is_none());
    server.verify().await;
    drop(client);
    drop(server);
}

#[tokio::test]
async fn test_read_catalog_projects_normalizes_and_deduplicates_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"},{"name":"FLASK"},{"name":"Django"}]}"#,
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();

    assert_eq!(read_catalog_projects(&client).await.unwrap(), ["django", "flask"]);
}

#[tokio::test]
async fn test_read_catalog_projects_reports_an_upstream_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();

    let error = read_catalog_projects(&client).await.unwrap_err();

    assert!(matches!(error, CatalogSyncError::Status(503)));
}

#[tokio::test]
async fn test_sync_catalog_coalesces_concurrent_fetches() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.pypi.simple.v1+json")
                .set_body_raw(
                    r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#,
                    "application/vnd.pypi.simple.v1+json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, meta) = store();
    let inflight = Inflight::default();

    let (first, second) = tokio::join!(
        sync_catalog(&client, &inflight, &meta, "concurrent", client.base_url()),
        sync_catalog(&client, &inflight, &meta, "concurrent", client.base_url())
    );

    assert!(matches!(first.unwrap(), CatalogSyncOutcome::Published { projects: 1 }));
    assert!(matches!(
        second.unwrap(),
        CatalogSyncOutcome::NotModified { projects: 1 }
    ));
}

#[tokio::test]
async fn test_sync_catalog_coalesces_concurrent_revalidations() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .and(header("if-none-match", "old"))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, meta) = store();
    seed_active(&meta, "concurrent-revalidation");
    let inflight = Inflight::default();

    let (first, second) = tokio::join!(
        sync_catalog(&client, &inflight, &meta, "concurrent-revalidation", client.base_url()),
        sync_catalog(&client, &inflight, &meta, "concurrent-revalidation", client.base_url())
    );

    assert!(matches!(
        first.unwrap(),
        CatalogSyncOutcome::NotModified { projects: 1 }
    ));
    assert!(matches!(
        second.unwrap(),
        CatalogSyncOutcome::NotModified { projects: 1 }
    ));
}

#[tokio::test]
async fn test_sync_catalog_304_sends_etag_and_merges_returned_validator() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .and(header("if-none-match", "old"))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "new"))
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, meta) = store();
    let generation = seed_active(&meta, "validated");

    assert!(matches!(
        sync_catalog(&client, &Inflight::default(), &meta, "validated", client.base_url())
            .await
            .unwrap(),
        CatalogSyncOutcome::NotModified { projects: 1 }
    ));

    let catalog = catalog_state(&meta, "validated").unwrap().active.unwrap();
    assert_eq!(catalog.generation, generation);
    assert_eq!(catalog.etag.as_deref(), Some("new"));
    assert_eq!(catalog.last_modified.as_deref(), Some("yesterday"));
}

#[tokio::test]
async fn test_sync_catalog_rejects_declared_oversized_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(Vec::new(), "application/vnd.pypi.simple.v1+json"))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, meta) = store();
    let mut response = client.head_index(CachedValidators::default()).await.unwrap();
    response.content_length = Some(MAX_CATALOG_BYTES + 1);

    let error = publish_response(&meta, "oversized", client.base_url(), response, 1)
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogSyncError::TooLarge));
    assert!(catalog_state(&meta, "oversized").unwrap().active.is_none());
}

#[tokio::test]
async fn test_sync_catalog_aborts_invalid_staging_generation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"bad name"}]}"#,
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, meta) = store();
    let active = seed_active(&meta, "invalid");

    let error = sync_catalog(&client, &Inflight::default(), &meta, "invalid", client.base_url())
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogSyncError::Json(error) if error.to_string().contains("bad name")));
    let state = catalog_state(&meta, "invalid").unwrap();
    assert_eq!(state.active.unwrap().generation, active);
    assert!(state.staging.is_none());
}

#[tokio::test]
async fn test_sync_catalog_rejects_response_without_projects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"meta":{"api-version":"1.4"}}"#,
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, meta) = store();
    let (generation, expected) = begin_catalog_generation(&meta, "no-projects").unwrap();
    put_catalog_projects(
        &meta,
        "no-projects",
        generation,
        &[("flask".to_owned(), "Flask".to_owned())],
    )
    .unwrap();
    let mut seeded = active(generation);
    seeded.projects = 1;
    publish_catalog_generation(&meta, "no-projects", expected, seeded).unwrap();

    let error = sync_catalog(&client, &Inflight::default(), &meta, "no-projects", client.base_url())
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogSyncError::Json(error) if error.to_string().contains("projects")));
    let state = catalog_state(&meta, "no-projects").unwrap();
    assert_eq!(state.active.unwrap().generation, generation);
    assert!(state.staging.is_none());
    assert_eq!(list_projects(&meta, "no-projects").unwrap(), vec!["Flask"]);
}

#[test]
fn test_write_catalog_stream_caps_unknown_length() {
    let mut output = Vec::new();
    let mut bytes = 0;

    write_catalog_chunk(&mut output, b"1234", &mut bytes, 7).unwrap();
    let error = write_catalog_chunk(&mut output, b"5678", &mut bytes, 7).unwrap_err();

    assert!(matches!(error, CatalogSyncError::TooLarge));
    assert_eq!(output, b"1234");
}

#[tokio::test]
async fn test_sync_catalog_caps_decompressed_body() {
    let server = MockServer::start().await;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&vec![b'a'; 1024 * 1024]).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < 100_000);
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.pypi.simple.v1+json")
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(compressed),
        )
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let response = client.head_index(CachedValidators::default()).await.unwrap();
    let mut output = Vec::new();

    let error = write_catalog_stream(response.into_stream(), &mut output, 100_000)
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogSyncError::TooLarge));
}

#[test]
fn test_parse_failures_never_replace_active_generation() {
    for (document, limit) in [
        (
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}"#,
            MAX_CATALOG_PROJECTS,
        ),
        (
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"one"},{"name":"two"}]}"#,
            1,
        ),
    ] {
        let (_dir, meta) = store();
        let active = seed_active(&meta, "failure");
        let (staging, _) = begin_catalog_generation(&meta, "failure").unwrap();
        let error = parse_catalog_with_limit(
            &mut Cursor::new(document),
            "json",
            &Url::parse("https://example.invalid/simple/").unwrap(),
            &mut GenerationSink::new(&meta, "failure", staging),
            limit,
        )
        .unwrap_err();
        abort_catalog_generation(&meta, "failure", staging).unwrap();

        assert!(matches!(
            error,
            CatalogSyncError::Json(_) | CatalogSyncError::TooManyProjects
        ));
        assert_eq!(
            catalog_state(&meta, "failure").unwrap().active.unwrap().generation,
            active
        );
        assert!(list_projects(&meta, "failure").unwrap().is_empty());
    }
}

#[test]
fn test_json_parser_validates_shapes_and_ignores_extensions() {
    for document in [
        r"[]",
        r#"{"meta":{"api-version":"1.4"},"projects":{}}"#,
        r#"{"meta":{"api-version":"1.4"}}"#,
    ] {
        let (_dir, meta) = store();
        let (generation, _) = begin_catalog_generation(&meta, "shape").unwrap();

        let error = parse_catalog_with_limit(
            &mut Cursor::new(document),
            "json",
            &Url::parse("https://example.invalid/simple/").unwrap(),
            &mut GenerationSink::new(&meta, "shape", generation),
            MAX_CATALOG_PROJECTS,
        )
        .unwrap_err();

        assert!(matches!(error, CatalogSyncError::Json(_)));
    }

    let (_dir, meta) = store();
    let (generation, _) = begin_catalog_generation(&meta, "extension").unwrap();
    let document = r#"{"extension":{"ignored":true},"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#;
    assert_eq!(
        parse_catalog_with_limit(
            &mut Cursor::new(document),
            "json",
            &Url::parse("https://example.invalid/simple/").unwrap(),
            &mut GenerationSink::new(&meta, "extension", generation),
            MAX_CATALOG_PROJECTS,
        )
        .unwrap(),
        1
    );
}

#[test]
fn test_catalog_batch_flushes_at_transaction_limit() {
    let (_dir, meta) = store();
    let (generation, _) = begin_catalog_generation(&meta, "batch").unwrap();
    let projects = (0..super::CATALOG_BATCH)
        .map(|index| format!(r#"{{"name":"project-{index}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let document = format!(r#"{{"meta":{{"api-version":"1.4"}},"projects":[{projects}]}}"#);

    assert_eq!(
        parse_catalog_with_limit(
            &mut Cursor::new(document),
            "json",
            &Url::parse("https://example.invalid/simple/").unwrap(),
            &mut GenerationSink::new(&meta, "batch", generation),
            MAX_CATALOG_PROJECTS,
        )
        .unwrap(),
        super::CATALOG_BATCH as u64
    );
}

#[test]
fn test_streaming_html_and_json_publish_equivalent_names() {
    for (format, document) in [
        (
            "json",
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"},{"name":"Django"}]}"#,
        ),
        (
            "html",
            r#"<!doctype html><meta name="pypi:repository-version" content="1.4"><a href="/simple/flask/">Flask</a><a href="/simple/django/">Django</a>"#,
        ),
    ] {
        let (_dir, meta) = store();
        let (generation, expected) = begin_catalog_generation(&meta, format).unwrap();
        let projects = parse_catalog_with_limit(
            &mut Cursor::new(document),
            format,
            &Url::parse("https://example.invalid/simple/").unwrap(),
            &mut GenerationSink::new(&meta, format, generation),
            MAX_CATALOG_PROJECTS,
        )
        .unwrap();
        let mut catalog = active(generation);
        catalog.projects = projects;
        publish_catalog_generation(&meta, format, expected, catalog).unwrap();
        assert_eq!(list_projects(&meta, format).unwrap(), vec!["Django", "Flask"]);
    }
}

#[test]
fn test_html_parser_uses_links_and_rejects_nameless_anchors() {
    let (_dir, meta) = store();
    let base = Url::parse("https://example.invalid/simple/").unwrap();
    let (generation, _) = begin_catalog_generation(&meta, "href").unwrap();
    assert_eq!(
        parse_catalog_with_limit(
            &mut Cursor::new(r#"</a><a href="/simple/flask/"></a>"#),
            "html",
            &base,
            &mut GenerationSink::new(&meta, "href", generation),
            MAX_CATALOG_PROJECTS,
        )
        .unwrap(),
        1
    );

    let (generation, _) = begin_catalog_generation(&meta, "nameless").unwrap();
    let error = parse_catalog_with_limit(
        &mut Cursor::new(r"<a></a><a>ignored after error</a>"),
        "html",
        &base,
        &mut GenerationSink::new(&meta, "nameless", generation),
        MAX_CATALOG_PROJECTS,
    )
    .unwrap_err();
    assert!(matches!(error, CatalogSyncError::MissingHtmlProjectName));
}

#[test]
fn test_html_tokenizer_accepts_decoder_errors() {
    let (_dir, meta) = store();
    let base = Url::parse("https://example.invalid/simple/").unwrap();
    let (generation, _) = begin_catalog_generation(&meta, "decoder").unwrap();
    let mut sink = GenerationSink::new(&meta, "decoder", generation);
    let mut batcher = CatalogBatcher::new(&mut sink, MAX_CATALOG_PROJECTS);
    let state = Rc::new(RefCell::new(HtmlState::new(&base, &mut batcher)));
    let mut tokenizer = HtmlTokenizer {
        tokenizer: html5ever::tokenizer::Tokenizer::new(
            HtmlSink {
                state: Rc::clone(&state),
            },
            html5ever::tokenizer::TokenizerOpts::default(),
        ),
    };

    html5ever::tendril::stream::TendrilSink::error(&mut tokenizer, "invalid input".into());
}

#[test]
fn test_redact_url_removes_request_secrets() {
    assert_eq!(
        redact_url("https://user:password@example.invalid/simple/?token=secret#fragment"),
        "https://example.invalid/simple/"
    );
    assert_eq!(redact_url("not a URL with a secret"), "<invalid-url>");
}
