use peryx_bench_core::report::load as load_report;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::super::test_support::{
    benchmark, http_client, metadata_bad_base, metadata_good_base, server, set_metadata_bases,
};
use super::*;

#[tokio::test]
async fn metadata_records_successful_and_failed_servers() {
    let good = MockServer::start().await;
    let bad = MockServer::start().await;
    mount_json_page(&good).await;
    set_metadata_bases(&good, &bad);
    let (directory, context) = benchmark();
    let servers = [server("good", metadata_good_base), server("bad", metadata_bad_base)];

    metadata(&context, &servers, 1, &http_client()).await.unwrap();

    let report = load_report(&directory.path().join("report.toml")).unwrap();
    let table = &report.tables["metadata"];
    assert_eq!(table.rows.len(), 5);
    for row in &table.rows[..3] {
        assert!(row.cells[0].value.is_some());
        assert_eq!(row.cells[1].text, "error");
    }
}

#[tokio::test]
async fn metadata_urls_parses_html_and_rejects_empty_pages() {
    let html = MockServer::start().await;
    Mock::given(path("/simple/boto3/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string(
                    r#"<a href="../../files/one.whl#hash" data-core-metadata="sha256=abc">one</a>
                       <a href="../../files/two.tar.gz" data-core-metadata="sha256=def">two</a>"#,
                ),
        )
        .mount(&html)
        .await;
    let client = http_client();
    assert_eq!(
        metadata_urls(&format!("{}/simple/", html.uri()), &client)
            .await
            .unwrap(),
        vec![format!("{}/files/one.whl.metadata", html.uri())]
    );

    let empty = MockServer::start().await;
    Mock::given(path("/simple/boto3/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<html></html>"),
        )
        .mount(&empty)
        .await;
    assert_eq!(
        metadata_urls(&format!("{}/simple/", empty.uri()), &client)
            .await
            .unwrap_err()
            .to_string(),
        "boto3 exposes no PEP 658 metadata URLs"
    );
}

#[rstest::rstest]
#[case::wheel("pkg-1-py3-none-any.whl", true)]
#[case::uppercase("pkg-1-py3-none-any.WHL?download=1#sha256=abc", true)]
#[case::source("pkg-1.tar.gz", false)]
fn wheel_paths_ignore_queries_and_fragments(#[case] path: &str, #[case] expected: bool) {
    assert_eq!(is_wheel_path(path), expected);
}

#[rstest::rstest]
#[case::boolean(serde_json::json!(true), true)]
#[case::false_boolean(serde_json::json!(false), false)]
#[case::hash(serde_json::json!({"sha256": "abc"}), true)]
#[case::missing(serde_json::Value::Null, false)]
fn metadata_presence_accepts_boolean_or_hash(#[case] value: serde_json::Value, #[case] expected: bool) {
    assert_eq!(metadata_present(&value), expected);
}

#[test]
fn json_metadata_urls_requires_a_files_array() {
    let page = url::Url::parse("https://index.example/simple/boto3/").unwrap();
    assert_eq!(
        json_metadata_urls(&page, r#"{"meta":{}}"#).unwrap_err().to_string(),
        "simple JSON has no files"
    );
}

async fn mount_json_page(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/simple/boto3/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                serde_json::json!({
                    "files": [
                        {
                            "filename": "boto3-1-py3-none-any.whl",
                            "url": "/files/boto3-1-py3-none-any.whl#sha256=abc",
                            "core-metadata": true
                        },
                        {
                            "filename": "boto3-1.tar.gz",
                            "url": "/files/boto3-1.tar.gz",
                            "core-metadata": true
                        }
                    ]
                })
                .to_string(),
                "application/vnd.pypi.simple.v1+json",
            ),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/boto3-1-py3-none-any.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Metadata-Version: 2.1\n"))
        .mount(server)
        .await;
}
