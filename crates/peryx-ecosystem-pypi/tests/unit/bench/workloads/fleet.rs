use peryx_bench_core::report::load as load_report;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::super::test_support::{
    WHEEL, benchmark, fleet_bad_base, fleet_good_base, http_client, server, set_fleet_bases, wheel_index,
};
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn fleet_workload_records_successful_and_failed_servers() {
    let good = wheel_index().await;
    let bad = MockServer::start().await;
    Mock::given(path("/simple/sample-pkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&bad)
        .await;
    set_fleet_bases(&good, &bad);
    let (directory, context) = benchmark();
    let servers = [server("good", fleet_good_base), server("bad", fleet_bad_base)];

    fleet_package(&context, &servers, 1, &http_client(), "sample-pkg", "python3", 1)
        .await
        .unwrap();

    let report = load_report(&directory.path().join("report.toml")).unwrap();
    let rows = &report.tables["parallel-install"].rows;
    assert_eq!(rows.len(), 4);
    for row in &rows[..2] {
        assert!(row.cells[0].value.is_some());
        assert_eq!(row.cells[1].text, "error");
    }
    let requests = good.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/simple/sample-pkg/")
            .count()
            >= 2
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == format!("/files/{WHEEL}"))
            .count()
            >= 2
    );
}
