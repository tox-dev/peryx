use std::process::Command;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use peryx_bench_core::report::load as load_report;

use super::super::super::test_support::{
    benchmark, endpoints_bad_base, endpoints_good_base, http_client, server, set_endpoints_bases,
};
use super::*;

#[tokio::test]
async fn endpoints_records_answered_and_failed_rounds() {
    let good = MockServer::start().await;
    let bad = MockServer::start().await;
    mount_endpoints(
        &good,
        &[
            "/simple/",
            "/boto3/json",
            "/files/sample.whl.metadata",
            "/inspect/sample.whl",
        ],
    )
    .await;
    set_endpoints_bases(&good, &bad);
    let (directory, context) = benchmark();
    let client = http_client();

    endpoints(&context, &[server("peryx", endpoints_good_base)], 1, &client)
        .await
        .unwrap();
    let successful = load_report(&directory.path().join("report.toml")).unwrap();
    let rows = &successful.tables["endpoints"].rows;
    assert_eq!(rows.len(), 9);
    assert!(rows[..7].iter().all(|row| row.cells[0].value.is_some()));

    endpoints(&context, &[server("peryx", endpoints_bad_base)], 1, &client)
        .await
        .unwrap();
    let failed = load_report(&directory.path().join("report.toml")).unwrap();
    assert!(
        failed.tables["endpoints"].rows[..7]
            .iter()
            .all(|row| row.cells[0].text == "error")
    );
}

#[tokio::test]
async fn endpoints_live_status_counts_answered_endpoints() {
    const CHILD: &str = "PERYX_ENDPOINTS_STATUS_CHILD";

    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "bench::workloads::endpoints::tests::endpoints_live_status_counts_answered_endpoints",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            (
                output.status.success(),
                stdout.lines().find(|line| line.starts_with("[endpoints] peryx: ")),
            ),
            (true, Some("[endpoints] peryx: 5/7 endpoints"))
        );
        return;
    }

    let mock = MockServer::start().await;
    mount_endpoints(&mock, &["/simple/", "/boto3/json"]).await;
    set_endpoints_bases(&mock, &mock);
    let (_directory, context) = benchmark();
    endpoints(&context, &[server("peryx", endpoints_good_base)], 1, &http_client())
        .await
        .unwrap();
}

#[tokio::test]
async fn endpoint_helpers_report_absent_and_invalid_pages() {
    let server = MockServer::start().await;
    let client = http_client();
    assert_eq!(probe(&client, &format!("{}/missing", server.uri()), "*/*").await, None);
    assert_eq!(
        endpoint_round(&format!("{}/wrong/", server.uri()), &client)
            .await
            .unwrap_err()
            .to_string(),
        "the server's index url does not end in simple/"
    );
    Mock::given(path("/simple/boto3/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    assert_eq!(
        endpoint_round(&format!("{}/simple/", server.uri()), &client)
            .await
            .unwrap_err()
            .to_string(),
        "boto3's page carried no file url"
    );
}

#[tokio::test]
async fn endpoint_round_reports_invalid_file_urls() {
    let server = MockServer::start().await;
    Mock::given(path("/simple/boto3/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"files":[{"url":"http://["}]}"#,
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;

    assert_eq!(
        endpoint_round(&format!("{}/simple/", server.uri()), &http_client())
            .await
            .unwrap_err()
            .to_string(),
        format!("http://[ is not a url relative to {}/simple/boto3/", server.uri())
    );
}

#[rstest::rstest]
#[case::json(
    r#"{"files":[{"url":"../files/demo.whl#sha256=abc"}]}"#,
    Some("../files/demo.whl#sha256=abc")
)]
#[case::html(r#"<a href="../files/demo.whl#sha256=abc">demo</a>"#, Some("../files/demo.whl"))]
#[case::missing("<html></html>", None)]
fn first_file_url_parses_both_simple_representations(#[case] page: &str, #[case] expected: Option<&str>) {
    assert_eq!(first_file_url(page).as_deref(), expected);
}

#[rstest::rstest]
#[case::artifact(
    "https://index.example/root/",
    "https://index.example/root/files/ab/demo.whl",
    "https://index.example/root/inspect/ab/demo.whl"
)]
#[case::missing(
    "https://index.example/root/",
    "https://files.example/demo.whl",
    "https://index.example/root/inspect/missing"
)]
fn inspect_url_maps_artifact_paths(#[case] root: &str, #[case] file: &str, #[case] expected: &str) {
    assert_eq!(inspect_url(root, file), expected);
}

async fn mount_endpoints(server: &MockServer, paths: &[&str]) {
    for request_path in paths {
        Mock::given(method("GET"))
            .and(path(*request_path))
            .respond_with(ResponseTemplate::new(200).set_body_string("response"))
            .mount(server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/simple/boto3/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                serde_json::json!({
                    "files": [{"url": format!("{}/files/sample.whl", server.uri())}]
                })
                .to_string(),
                "application/vnd.pypi.simple.v1+json",
            ),
        )
        .mount(server)
        .await;
}
