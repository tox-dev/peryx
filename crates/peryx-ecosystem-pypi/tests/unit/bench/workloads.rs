use std::process::Command;

use peryx_bench_core::usage::Usage;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::test_support::http_client;
use super::*;

#[test]
fn rounds_records_server_costs_and_omits_direct_costs() {
    let mut rounds = Rounds::new();
    rounds.record_cost(Usage::watch(None).unwrap()).unwrap();
    assert_eq!(rounds.costs(), None);

    let mut rounds = Rounds::new();
    rounds
        .record_cost(Usage::watch(Some(std::process::id())).unwrap())
        .unwrap();
    let costs = rounds.costs().unwrap();
    assert_eq!(costs.len(), 1);
    assert!(costs[0].peak_rss_bytes > 0);
}

#[rstest::rstest]
#[case::empty(&[], "-")]
#[case::median(&[1.0, 3.0, 2.0], "2")]
fn medians_format_samples(#[case] samples: &[f64], #[case] expected_rate: &str) {
    assert_eq!(median_or_dash_rate(samples), expected_rate);
}

#[test]
fn checked_command_reports_each_outcome() {
    run_checked(&mut Command::new("true")).unwrap();
    assert!(
        run_checked(&mut Command::new("false"))
            .unwrap_err()
            .to_string()
            .contains("failed")
    );
    assert_eq!(
        run_checked(&mut Command::new("peryx-missing-command"))
            .unwrap_err()
            .to_string(),
        "command did not start"
    );
}

#[tokio::test]
async fn drain_checks_status_and_consumes_the_body() {
    let server = MockServer::start().await;
    Mock::given(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1; 32_768]))
        .mount(&server)
        .await;
    Mock::given(path("/error"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = http_client();

    drain(client.get(format!("{}/ok", server.uri())).send().await.unwrap())
        .await
        .unwrap();
    assert_eq!(
        drain(client.get(format!("{}/error", server.uri())).send().await.unwrap())
            .await
            .unwrap_err()
            .to_string(),
        format!(
            "HTTP status server error (503 Service Unavailable) for url ({}/error)",
            server.uri()
        )
    );
}
