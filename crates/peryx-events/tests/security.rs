use std::io::{Read as _, Seek as _};
use std::sync::Mutex;

use http::{HeaderMap, HeaderValue, header};
use peryx_events::security::{
    Attribution, AuthorizationDenial, Event, RequestContext, RoleGrantChange, authorization_denied, role_grant_change,
};
use peryx_identity::{Identity, Principal};
use rstest::rstest;

#[rstest]
#[case::grant(RoleGrantChange::Grant, "grant")]
#[case::revoke(RoleGrantChange::Revoke, "revoke")]
fn test_role_grant_event_records_only_bounded_delegation_context(
    #[case] change: RoleGrantChange,
    #[case] expected_action: &str,
) {
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    let target = peryx_identity::UserId::random();

    tracing::subscriber::with_default(subscriber, || {
        role_grant_change(
            Some("alice"),
            change,
            &target,
            peryx_identity::Role::RepositoryReader,
            "repository/team/api",
            "allowed",
            "created",
        );
    });

    capture.rewind().unwrap();
    let mut text = String::new();
    capture.read_to_string(&mut text).unwrap();
    let event: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "role grant mutation",
            "security_event": true,
            "event": "role_grant",
            "action": expected_action,
            "actor": "alice",
            "target": target.as_str(),
            "role": "repository_reader",
            "reach": "repository/team/api",
            "result": "allowed",
            "reason": "created",
        })
    );
}

/// The shape `IndexAcl::identify` returns for a Basic request: a password that matched `token`
/// alongside whatever username the client chose to type.
fn identified(token: Option<&str>, presented: Option<&str>) -> Identity {
    Identity {
        principal: token.map_or(Principal::Anonymous, |subject| Principal::Named {
            subject: subject.to_owned(),
        }),
        presented_user: presented.map(str::to_owned),
    }
}

/// A matched token authorizes on its secret alone, so the username beside it names nobody.
#[test]
fn test_attribution_credits_the_matched_token_not_the_presented_username() {
    let attribution = Attribution::resolve(&identified(Some("release-bot"), Some("someone-else")));

    assert_eq!(
        (attribution.actor(), attribution.presented_user()),
        (Some("release-bot"), Some("someone-else"))
    );
}

#[test]
fn test_attribution_leaves_a_failed_authentication_without_an_actor() {
    let attribution = Attribution::resolve(&identified(None, Some("alice")));

    assert_eq!(
        (attribution.actor(), attribution.presented_user()),
        (None, Some("alice"))
    );
}

#[test]
fn test_attribution_credits_a_bearer_principal_that_presented_no_username() {
    let attribution = Attribution::resolve(&identified(Some("ci"), None));

    assert_eq!((attribution.actor(), attribution.presented_user()), (Some("ci"), None));
}

#[test]
fn test_attribution_is_empty_for_an_anonymous_request() {
    let attribution = Attribution::resolve(&identified(None, None));

    assert_eq!((attribution.actor(), attribution.presented_user()), (None, None));
}

#[rstest]
#[case::control_characters("al\nice\u{0}\r\u{7}", "alice")]
#[case::over_the_bound(&"a".repeat(100), &"a".repeat(64))]
#[case::empty("", "")]
fn test_attribution_bounds_the_presented_username(#[case] presented: &str, #[case] expected: &str) {
    let attribution = Attribution::resolve(&identified(Some("release-bot"), Some(presented)));

    assert_eq!(attribution.presented_user(), Some(expected));
}

#[rstest]
#[case::no_grant(AuthorizationDenial::NoGrant, "no_grant")]
#[case::storage_unavailable(AuthorizationDenial::StorageUnavailable, "storage_unavailable")]
fn test_authorization_denial_event_contains_only_bounded_context(
    #[case] denial: AuthorizationDenial,
    #[case] expected_reason: &str,
) {
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    let user = peryx_identity::UserId::random();

    tracing::subscriber::with_default(subscriber, || {
        authorization_denied(&user, peryx_identity::Scope::OperatorRead, denial);
    });

    capture.rewind().unwrap();
    let mut text = String::new();
    capture.read_to_string(&mut text).unwrap();
    let event: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "role authorization denied",
            "security_event": true,
            "event": "authorization",
            "user": user.as_str(),
            "scope": "operator:read",
            "result": "denied",
            "reason": expected_reason,
        })
    );
}

#[test]
fn test_index_action_event_records_all_bounded_context() {
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("request-1"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("client/1"));

    tracing::subscriber::with_default(subscriber, || {
        Event::new("write", "allowed")
            .actor(Some("alice"))
            .token_id("token-1")
            .index("virtual")
            .source_index("cached")
            .hosted_index("hosted")
            .resource(Some("demo"))
            .group(Some("1.0"))
            .artifact(Some("artifact.bin"))
            .digest(Some("sha256"))
            .count(2)
            .changed(true)
            .reason(Some("accepted"))
            .request(RequestContext::new(&headers, Some("203.0.113.9".parse().unwrap())))
            .emit();
    });

    capture.rewind().unwrap();
    let mut text = String::new();
    capture.read_to_string(&mut text).unwrap();
    let event: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "index security event",
            "security_event": true,
            "event": "index_action",
            "action": "write",
            "result": "allowed",
            "actor": "alice",
            "presented_user": "",
            "token_id": "token-1",
            "index": "virtual",
            "source_index": "cached",
            "hosted_index": "hosted",
            "resource": "demo",
            "group": "1.0",
            "artifact": "artifact.bin",
            "digest": "sha256",
            "count": 2,
            "changed": true,
            "reason": "accepted",
            "request_id": "request-1",
            "user_agent": "client/1",
            "client_ip": "203.0.113.9",
        })
    );
}

#[test]
fn test_index_action_event_discards_non_text_headers() {
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_bytes(&[0xff]).unwrap());

    tracing::subscriber::with_default(subscriber, || {
        Event::new("delete", "denied")
            .request(RequestContext::new(&headers, None))
            .emit();
    });

    capture.rewind().unwrap();
    let mut text = String::new();
    capture.read_to_string(&mut text).unwrap();
    let event: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "index security event",
            "security_event": true,
            "event": "index_action",
            "action": "delete",
            "result": "denied",
            "actor": "",
            "presented_user": "",
            "token_id": "",
            "index": "",
            "source_index": "",
            "hosted_index": "",
            "resource": "",
            "group": "",
            "artifact": "",
            "digest": "",
            "count": 0,
            "changed": false,
            "reason": "",
            "request_id": "",
            "user_agent": "",
            "client_ip": "",
        })
    );
}

/// An operator reads `actor` to decide which credential to revoke, so the two names stay in
/// separate fields even when the record carries both.
#[test]
fn test_index_action_event_separates_the_verified_actor_from_the_presented_username() {
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    let attribution = Attribution::resolve(&identified(Some("release-bot"), Some("someone-else")));

    tracing::subscriber::with_default(subscriber, || {
        Event::new("upload", "success").attribution(&attribution).emit();
    });

    capture.rewind().unwrap();
    let mut text = String::new();
    capture.read_to_string(&mut text).unwrap();
    let event: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        (&event["fields"]["actor"], &event["fields"]["presented_user"]),
        (
            &serde_json::Value::from("release-bot"),
            &serde_json::Value::from("someone-else")
        )
    );
}
