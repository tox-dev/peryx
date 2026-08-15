use peryx_upstream::{NamedUpstream, Reachability, UpstreamClient, UpstreamHealth};
use rstest::rstest;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_upstream_client_clones_share_reachability_transitions() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let reachable = UpstreamClient::new(&server.uri()).unwrap();
    let unreachable = UpstreamClient::new("http://127.0.0.1:0/").unwrap();
    let reachable_clone = reachable.clone();
    let unreachable_clone = unreachable.clone();

    assert_eq!(
        (reachable.reachability(), unreachable.reachability()),
        (Reachability::Unknown, Reachability::Unknown)
    );

    tokio::join!(reachable_clone.warm(), unreachable_clone.warm());

    assert_eq!(
        (reachable.reachability(), unreachable.reachability()),
        (Reachability::Reachable, Reachability::Unreachable)
    );
}

#[rstest]
#[case::healthy(NamedUpstream::mark_healthy, UpstreamHealth::Healthy)]
#[case::unhealthy(NamedUpstream::mark_unhealthy, UpstreamHealth::Unhealthy)]
fn test_named_upstream_records_health_transitions(
    #[case] update: fn(&NamedUpstream),
    #[case] expected: UpstreamHealth,
) {
    let upstream = NamedUpstream::new("primary", UpstreamClient::new("https://example.invalid/").unwrap());
    assert_eq!(upstream.health(), UpstreamHealth::Configured);

    update(&upstream);

    assert_eq!(upstream.health(), expected);
}
