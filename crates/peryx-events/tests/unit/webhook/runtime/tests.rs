use rstest::rstest;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

type ErrorMatch = fn(&WebhookConfigError) -> bool;
const ALLOWED_EVENTS: &[&str] = &["management", "resource-write"];

const SECRET: &str = "test-webhook-signing-secret-32-bytes";

fn target(name: &str, url: &str, secret: &str, events: &[&str]) -> WebhookTargetConfig {
    WebhookTargetConfig {
        index: "hosted".to_owned(),
        name: name.to_owned(),
        url: url.to_owned(),
        secret: secret.to_owned(),
        events: events.iter().map(|&event| event.to_owned()).collect(),
        allowed_events: ALLOWED_EVENTS,
    }
}

#[test]
fn test_runtime_matches_all_events_when_no_filter_is_set() {
    let runtime = WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", SECRET, &[])]).unwrap();

    assert_eq!(runtime.target_names("hosted", "resource-write"), ["ci"]);
    assert_eq!(runtime.target_names("hosted", "management"), ["ci"]);
    assert!(runtime.target_names("other", "resource-write").is_empty());
}

#[rstest]
#[case::empty_name(vec![target("", "https://ci.example/hook", SECRET, &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::EmptyName { .. }))]
#[case::duplicate(
    vec![
        target("ci", "https://ci.example/hook", SECRET, &[]),
        target("ci", "https://ci.example/other", SECRET, &[]),
    ],
    |err: &WebhookConfigError| matches!(err, WebhookConfigError::Duplicate { .. })
)]
#[case::invalid_url(vec![target("ci", "not a url", SECRET, &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::InvalidUrl { .. }))]
#[case::invalid_scheme(vec![target("ci", "file:///tmp/hook", SECRET, &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::InvalidScheme { .. }))]
#[case::credentials(vec![target("ci", "https://user@ci.example/hook", SECRET, &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::SensitiveUrlParts { .. }))]
#[case::sensitive_url_parts(vec![target("ci", "https://ci.example/hook?token=secret", SECRET, &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::SensitiveUrlParts { .. }))]
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
#[case::one_byte(1)]
#[case::below_minimum(31)]
fn test_runtime_rejects_undersized_secrets(#[case] length: usize) {
    let secret = "z".repeat(length);

    let error = WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", &secret, &[])])
        .err()
        .expect("undersized secret must fail");
    let message = error.to_string();

    assert_eq!(
        (message.as_str(), message.contains(&secret)),
        (
            "webhook target ci on index hosted secret must contain at least 32 bytes",
            false,
        )
    );
}

#[test]
fn test_runtime_accepts_a_32_byte_secret() {
    WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", &"x".repeat(32), &[])]).unwrap();
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
fn test_runtime_rejects_invalid_event_name() {
    assert!(matches!(
        WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", SECRET, &["Bad event"])]),
        Err(WebhookConfigError::UnknownEvent(event)) if event == "Bad event"
    ));
}

#[test]
fn test_runtime_rejects_event_the_owner_does_not_emit() {
    assert!(matches!(
        WebhookRuntime::new(vec![target(
            "ci",
            "https://ci.example/hook",
            SECRET,
            &["resource-delete"]
        )]),
        Err(WebhookConfigError::UnknownEvent(event)) if event == "resource-delete"
    ));
}

#[rstest]
#[case("management")]
#[case("resource-write")]
fn test_runtime_accepts_each_owner_event(#[case] event: &str) {
    let runtime = WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", SECRET, &[event])]).unwrap();

    assert_eq!(runtime.target_names("hosted", event), ["ci"]);
}
