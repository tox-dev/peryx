use peryx_bench_core::report::load as load_report;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::super::test_support::{
    benchmark, http_client, server, set_throughput_bases, throughput_bad_base, throughput_good_base,
};
use super::*;

#[cfg(target_os = "macos")]
const PLATFORM_WHEEL: &str = "torch-1.0-macosx_arm64.whl";
#[cfg(not(target_os = "macos"))]
const PLATFORM_WHEEL: &str = "torch-1.0-manylinux_x86_64.whl";

#[tokio::test]
async fn throughput_records_transfers_and_failed_servers() {
    let good = MockServer::start().await;
    let bad = MockServer::start().await;
    mount_wheel(&good, PLATFORM_WHEEL).await;
    set_throughput_bases(&good, &bad);
    let (directory, context) = benchmark();
    let servers = [server("good", throughput_good_base), server("bad", throughput_bad_base)];

    throughput_from(
        &context,
        &servers,
        1,
        &http_client(),
        &format!("{}/simple/", good.uri()),
    )
    .await
    .unwrap();

    let report = load_report(&directory.path().join("report.toml")).unwrap();
    let rows = &report.tables["throughput"].rows;
    assert_eq!(rows.len(), 5);
    for row in &rows[..3] {
        assert!(row.cells[0].value.is_some());
        assert_eq!(row.cells[1].text, "error");
    }
    let requests = good.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/simple/torch/")
            .count()
            >= 2
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == format!("/files/{PLATFORM_WHEEL}"))
            .count()
            >= 13
    );
}

#[tokio::test]
async fn wheel_url_accepts_html_and_reports_missing_artifacts() {
    let server = MockServer::start().await;
    Mock::given(path("/html/torch/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string(r#"<a href="../../files/torch.whl#sha256=abc">wheel</a>"#),
        )
        .mount(&server)
        .await;
    Mock::given(path("/json/torch/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            serde_json::json!({"files": []}).to_string(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let client = http_client();
    assert_eq!(
        wheel_url(&format!("{}/html/", server.uri()), "torch", "torch.whl", &client,)
            .await
            .unwrap(),
        format!("{}/files/torch.whl", server.uri())
    );
    assert_eq!(
        wheel_url(&format!("{}/json/", server.uri()), "torch", "torch.whl", &client,)
            .await
            .unwrap_err()
            .to_string(),
        "wheel missing from the JSON page"
    );
    assert_eq!(
        wheel_url(&format!("{}/html/", server.uri()), "torch", "missing.whl", &client,)
            .await
            .unwrap_err()
            .to_string(),
        "wheel missing from the HTML page"
    );
}

#[tokio::test]
async fn stress_wheel_requires_files_for_the_platform() {
    let missing_files = MockServer::start().await;
    Mock::given(path("/simple/torch/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&missing_files)
        .await;
    let client = http_client();
    assert_eq!(
        stress_wheel_filename(&format!("{}/simple/", missing_files.uri()), &client)
            .await
            .unwrap_err()
            .to_string(),
        "simple JSON has no files"
    );

    let wrong_platform = MockServer::start().await;
    Mock::given(path("/simple/torch/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"files": [{"filename": "torch-win_amd64.whl"}]})),
        )
        .mount(&wrong_platform)
        .await;
    assert_eq!(
        stress_wheel_filename(&format!("{}/simple/", wrong_platform.uri()), &client)
            .await
            .unwrap_err()
            .to_string(),
        "no wheel matches this platform"
    );
}

#[rstest::rstest]
#[case::match_first(r#"<a href="one.whl">one</a><a href="two.whl">two</a>"#, "one.whl", Some("one.whl"))]
#[case::match_second(r#"<a href="one.whl">one</a><a href="two.whl">two</a>"#, "two.whl", Some("two.whl"))]
#[case::missing("<html></html>", "one.whl", None)]
fn html_href_finds_the_requested_target(#[case] body: &str, #[case] filename: &str, #[case] expected: Option<&str>) {
    assert_eq!(html_href(body, filename).as_deref(), expected);
}

async fn mount_wheel(server: &MockServer, filename: &str) {
    Mock::given(method("GET"))
        .and(path("/simple/torch/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                serde_json::json!({
                    "files": [{"filename": filename, "url": format!("/files/{filename}#sha256=abc")}]
                })
                .to_string(),
                "application/vnd.pypi.simple.v1+json",
            ),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7; 32_768]))
        .mount(server)
        .await;
}
