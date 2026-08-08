use std::io::{Read as _, Seek as _};
use std::sync::Mutex;

use http::{HeaderMap, HeaderValue, header};
use peryx_identity::{Identity, Principal};
use rstest::rstest;

use super::{AuthorizationDenial, RoleGrantChange};

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
        super::role_grant_change(
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

fn presenting(user: &str) -> Identity {
    Identity {
        principal: Principal::Anonymous,
        user: Some(user.to_owned()),
    }
}

#[test]
fn test_actor_uses_the_presented_username() {
    assert_eq!(super::actor(&presenting("alice")).as_deref(), Some("alice"));
}

#[test]
fn test_actor_calls_an_empty_username_unknown() {
    assert_eq!(super::actor(&presenting("")).as_deref(), Some("unknown"));
}

#[test]
fn test_actor_falls_back_to_the_principal_when_no_username_was_presented() {
    let bearer = Identity {
        principal: Principal::Named {
            subject: "ci".to_owned(),
        },
        user: None,
    };
    assert_eq!(super::actor(&bearer).as_deref(), Some("ci"));
}

#[test]
fn test_actor_is_none_for_an_anonymous_request() {
    let anonymous = Identity {
        principal: Principal::Anonymous,
        user: None,
    };
    assert_eq!(super::actor(&anonymous), None);
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
        super::authorization_denied(&user, peryx_identity::Scope::OperatorRead, denial);
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
        super::Event::new("upload", "allowed")
            .actor(Some("alice"))
            .publisher_id("publisher-1")
            .token_id("token-1")
            .index("virtual")
            .source_index("cached")
            .hosted_index("hosted")
            .project(Some("demo"))
            .version(Some("1.0"))
            .filename(Some("demo.whl"))
            .digest(Some("sha256"))
            .count(2)
            .changed(true)
            .reason(Some("accepted"))
            .request(&headers)
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
            "action": "upload",
            "result": "allowed",
            "actor": "alice",
            "publisher_id": "publisher-1",
            "token_id": "token-1",
            "index": "virtual",
            "source_index": "cached",
            "hosted_index": "hosted",
            "project": "demo",
            "version": "1.0",
            "filename": "demo.whl",
            "digest": "sha256",
            "count": 2,
            "changed": true,
            "reason": "accepted",
            "request_id": "request-1",
            "user_agent": "client/1",
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
        super::Event::new("delete", "denied").request(&headers).emit();
    });

    capture.rewind().unwrap();
    let mut text = String::new();
    capture.read_to_string(&mut text).unwrap();
    let event: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(event["fields"]["request_id"], "");
    assert_eq!(event["fields"]["user_agent"], "");
}
