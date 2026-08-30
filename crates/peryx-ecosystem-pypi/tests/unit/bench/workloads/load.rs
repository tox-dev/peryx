use std::path::Path;
use std::process::{Command, Stdio};

use peryx_bench_core::report::load as load_report;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::super::test_support::{
    benchmark, http_client, install_crypto_provider, load_bad_base, load_good_base, server, set_load_bases,
};
use super::*;

#[rstest::rstest]
#[case::server(true, Some(vec![3, 5]))]
#[case::direct(false, None)]
fn requests_align_with_server_costs(#[case] server_ran: bool, #[case] expected: Option<Vec<u64>>) {
    assert_eq!(requests_if_server_ran(server_ran, vec![3, 5]), expected);
}

#[tokio::test]
async fn load_workload_records_successful_and_failed_servers() {
    let good = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("page"))
        .mount(&good)
        .await;
    let bad = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&bad)
        .await;
    set_load_bases(&good, &bad);
    let (directory, context) = benchmark();
    let servers = [
        Server {
            name: "good",
            homepage: "https://example.invalid/",
            base_url: load_good_base,
            probe: |url| url.to_owned(),
            command: Some(idle_process),
            setup: None,
            teardown: None,
        },
        server("bad", load_bad_base),
    ];
    let windows = LoadWindows {
        capacity: Duration::from_millis(250),
        latency: Duration::from_millis(250),
    };

    load_with_windows(&context, &servers, &[1, 2], 1, &http_client(), &windows)
        .await
        .unwrap();

    let report = load_report(&directory.path().join("report.toml")).unwrap();
    let rows = &report.tables["load"].rows;
    assert_eq!(
        rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
        [
            "1 user: requests/s",
            "1 user: p95 latency",
            "2 users: requests/s",
            "2 users: p95 latency",
            "server CPU per 1k requests",
            "server peak memory",
        ]
    );
    for row in &rows[..4] {
        assert!(row.cells[0].value.is_some());
        assert_eq!(row.cells[1].text, "error");
    }
    assert!(rows[4].cells[0].value.is_some());
}

fn idle_process(_: &BenchmarkContext, _: u16, _: &Path) -> Command {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.args(["-c", "read value"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "set /p value="]);
        command
    };
    command.stdin(Stdio::piped());
    command
}

#[tokio::test]
async fn swarm_and_tail_report_empty_probes() {
    install_crypto_provider();
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let windows = LoadWindows {
        capacity: Duration::from_millis(20),
        latency: Duration::from_millis(20),
    };
    assert_eq!(
        swarm(&format!("{}/simple/", server.uri()), 1, &windows)
            .await
            .err()
            .unwrap()
            .to_string(),
        "the swarm completed no requests"
    );
    assert_eq!(
        measure_tail(&format!("{}/simple/", server.uri()), 0, 1.0, Duration::from_millis(1),)
            .await
            .unwrap_err()
            .to_string(),
        "the latency probe recorded no requests"
    );
}
