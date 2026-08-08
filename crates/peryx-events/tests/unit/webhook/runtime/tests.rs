use rstest::rstest;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

type ErrorMatch = fn(&WebhookConfigError) -> bool;

fn target(name: &str, url: &str, secret: &str, events: &[&str]) -> WebhookTargetConfig {
    WebhookTargetConfig {
        index: "hosted".to_owned(),
        name: name.to_owned(),
        url: url.to_owned(),
        secret: secret.to_owned(),
        events: events.iter().map(|&event| event.to_owned()).collect(),
    }
}

#[test]
fn test_runtime_matches_all_events_when_no_filter_is_set() {
    let runtime = WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", "secret", &[])]).unwrap();

    assert_eq!(runtime.target_names("hosted", WebhookEventKind::Upload), ["ci"]);
    assert_eq!(runtime.target_names("hosted", WebhookEventKind::Management), ["ci"]);
    assert!(runtime.target_names("other", WebhookEventKind::Upload).is_empty());
}

#[rstest]
#[case::empty_name(vec![target("", "https://ci.example/hook", "secret", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::EmptyName { .. }))]
#[case::empty_secret(vec![target("ci", "https://ci.example/hook", "", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::EmptySecret { .. }))]
#[case::duplicate(
    vec![
        target("ci", "https://ci.example/hook", "secret", &[]),
        target("ci", "https://ci.example/other", "secret", &[]),
    ],
    |err: &WebhookConfigError| matches!(err, WebhookConfigError::Duplicate { .. })
)]
#[case::invalid_url(vec![target("ci", "not a url", "secret", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::InvalidUrl { .. }))]
#[case::invalid_scheme(vec![target("ci", "file:///tmp/hook", "secret", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::InvalidScheme { .. }))]
#[case::sensitive_url_parts(vec![target("ci", "https://ci.example/hook?token=secret", "secret", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::SensitiveUrlParts { .. }))]
fn test_runtime_rejects_invalid_target_config(
    #[case] configs: Vec<WebhookTargetConfig>,
    #[case] matches_error: ErrorMatch,
) {
    let Err(err) = WebhookRuntime::new(configs) else {
        panic!("expected an invalid-config error");
    };
    assert!(matches_error(&err));
}

#[rstest]
#[case(301)]
#[case(302)]
#[case(307)]
#[case(308)]
#[tokio::test]
async fn test_delivery_client_surfaces_redirects_instead_of_following_them(#[case] status: u16) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status).insert_header("location", "/followed"))
        .expect(1)
        .mount(&server)
        .await;

    let response = WebhookRuntime::disabled()
        .client
        .post(server.uri())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), status);
}

#[test]
fn test_runtime_rejects_unknown_event() {
    assert!(matches!(
        WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", "secret", &["bogus"])]),
        Err(WebhookConfigError::UnknownEvent(event)) if event == "bogus"
    ));
}
