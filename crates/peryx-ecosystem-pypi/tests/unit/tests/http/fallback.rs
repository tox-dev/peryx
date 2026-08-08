//! Project-level source selection for virtual `PyPI` repositories.

use peryx_policy::FallbackMode;

use super::support::*;

const HOSTED_FILE: &str = "acme_pkg-1.0-py3-none-any.whl";
const UPSTREAM_FILE: &str = "acme_pkg-2.0-py3-none-any.whl";
const UPSTREAM_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn fallback_harness(mode: FallbackMode, protected: bool) -> Harness {
    let overlay_policy = policy(|neutral, pypi| {
        pypi.fallback_mode = mode;
        if protected {
            neutral.protected_names = vec!["acme-pkg".to_owned()];
        }
    });
    harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await
}

async fn mount_upstream(harness: &Harness) {
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"acme-pkg\",\"versions\":[\"2.0\"],\
         \"files\":[{{\"filename\":\"{UPSTREAM_FILE}\",\
         \"url\":\"https://upstream.invalid/{UPSTREAM_FILE}\",\
         \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path("/simple/acme-pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&harness.server)
        .await;
}

fn put_hosted(harness: &Harness) {
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
}

async fn upstream_request_count(harness: &Harness) -> usize {
    harness
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/simple/acme-pkg/")
        .count()
}

#[tokio::test]
async fn test_fallback_serves_upstream_when_hosted_project_is_missing() {
    let harness = fallback_harness(FallbackMode::Fallback, false).await;
    mount_upstream(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/acme-pkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(UPSTREAM_FILE));
    assert_eq!(upstream_request_count(&harness).await, 1);
}

#[tokio::test]
async fn test_fallback_merges_distinct_hosted_and_upstream_files() {
    let harness = fallback_harness(FallbackMode::Fallback, false).await;
    put_hosted(&harness);
    mount_upstream(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/acme-pkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(HOSTED_FILE));
    assert!(body.contains(UPSTREAM_FILE));
}

#[tokio::test(flavor = "current_thread")]
async fn test_private_first_records_and_hides_an_upstream_collision() {
    let harness = fallback_harness(FallbackMode::PrivateFirst, false).await;
    put_hosted(&harness);
    mount_upstream(&harness).await;
    let logs = LogCapture::default();
    let guard = logs.install();

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/acme-pkg/", Some("application/json")).await;

    drop(guard);
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(HOSTED_FILE));
    assert!(!body.contains(UPSTREAM_FILE));
    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "event") == Some("policy_decision"))
        .unwrap();
    assert_eq!(field(&event, "result"), Some("shadowed"));
    assert_eq!(field(&event, "index"), Some("root/pypi"));
    assert_eq!(field(&event, "project"), Some("acme-pkg"));
    assert_eq!(field(&event, "fallback_mode"), Some("private-first"));
    assert_eq!(field(&event, "hosted_members"), Some("hosted"));
    assert_eq!(field(&event, "cached_members"), Some("pypi"));
}

#[tokio::test]
async fn test_private_first_uses_upstream_when_hosted_project_is_missing() {
    let harness = fallback_harness(FallbackMode::PrivateFirst, false).await;
    mount_upstream(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/acme-pkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(UPSTREAM_FILE));
}

#[tokio::test]
async fn test_no_fallback_serves_hosted_without_calling_upstream() {
    let harness = fallback_harness(FallbackMode::NoFallback, false).await;
    put_hosted(&harness);
    mount_upstream(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/acme-pkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(HOSTED_FILE));
    assert_eq!(upstream_request_count(&harness).await, 0);
}

#[tokio::test]
async fn test_no_fallback_denies_a_missing_hosted_project_without_calling_upstream() {
    let harness = fallback_harness(FallbackMode::NoFallback, false).await;
    mount_upstream(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/acme-pkg/", Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "cached");
    assert_eq!(denial["project"], "acme-pkg");
    assert_eq!(denial["rule"], "virtual-fallback");
    assert_eq!(denial["field"], "fallback_mode");
    assert!(denial["reason"].as_str().unwrap().contains("no-fallback"));
    assert!(denial["reason"].as_str().unwrap().contains("hosted"));
    assert!(denial["reason"].as_str().unwrap().contains("pypi"));
    assert!(!body.contains("upstream.invalid"));
    assert_eq!(upstream_request_count(&harness).await, 0);
}

#[rstest]
#[case(FallbackMode::Fallback)]
#[case(FallbackMode::PrivateFirst)]
#[case(FallbackMode::NoFallback)]
#[tokio::test]
async fn test_protected_name_precedes_fallback_mode(#[case] mode: FallbackMode) {
    let harness = fallback_harness(mode, true).await;
    mount_upstream(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/acme-pkg/", Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["rule"], "protected-name");
    assert_eq!(upstream_request_count(&harness).await, 0);
}

#[rstest]
#[case("acme-pkg", "application/json")]
#[case("acme_pkg", "application/json")]
#[case("acme.pkg", "application/json")]
#[case("acme-pkg", "text/html")]
#[case("acme_pkg", "text/html")]
#[case("acme.pkg", "text/html")]
#[tokio::test]
async fn test_private_first_normalizes_project_names(#[case] project: &str, #[case] accept: &str) {
    let harness = fallback_harness(FallbackMode::PrivateFirst, false).await;
    put_hosted(&harness);
    mount_upstream(&harness).await;

    let (status, _, body) = get(&harness.state, &format!("/root/pypi/simple/{project}/"), Some(accept)).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(HOSTED_FILE));
    assert!(!body.contains(UPSTREAM_FILE));
}
