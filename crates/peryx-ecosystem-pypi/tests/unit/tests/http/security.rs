use super::support::*;
use super::upload::{token_basic, trusted_publishing, trusted_token};

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_upload_success_without_token_secret() {
    let h = harness().await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await,
        StatusCode::OK
    );

    drop(guard);
    let text = logs.text();
    assert!(!text.contains("s3cret"));
    let events = logs.security_events();
    assert!(events.iter().any(|event| {
        field(event, "action") == Some("token_use")
            && field(event, "result") == Some("success")
            && field(event, "actor") == Some("uploader")
            && field(event, "presented_user") == Some("__token__")
            && field(event, "index") == Some("hosted")
    }));
    let upload = events
        .iter()
        .find(|event| field(event, "action") == Some("upload") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(upload, "index"), Some("root/pypi"));
    assert_eq!(field(upload, "hosted_index"), Some("hosted"));
    assert_eq!(field(upload, "resource"), Some("peryxpkg"));
    assert_eq!(field(upload, "group"), Some("1.0"));
    assert_eq!(field(upload, "artifact"), Some("peryxpkg-1.0-py3-none-any.whl"));
    assert_eq!(upload["fields"]["count"], 1);
    assert!(field(upload, "digest").is_some_and(|digest| digest.len() == 64));
}
#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_invalid_token_without_secret() {
    let h = harness().await;
    let (content_type, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", b"x")));
    let auth = format!("Basic {}", STANDARD.encode("alice:nope"));
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&auth), &content_type, body).await,
        StatusCode::UNAUTHORIZED
    );

    drop(guard);
    let text = logs.text();
    assert!(!text.contains("nope"));
    assert!(!text.contains("s3cret"));
    let events = logs.security_events();
    let token = events
        .iter()
        .find(|event| field(event, "action") == Some("token_use") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(
        (field(token, "actor"), field(token, "presented_user")),
        (Some(""), Some("alice"))
    );
    assert_eq!(field(token, "index"), Some("hosted"));
    assert_eq!(field(token, "reason"), Some("invalid upload token"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_trusted_token_id_without_the_token() {
    let (_dir, state, signer) = trusted_publishing();
    let token = trusted_token(&signer, "hosted/peryxpkg");
    let token_id = signer
        .verify_scoped(&token, peryx_identity::TokenScope::new("trusted-publishing"))
        .unwrap()
        .id;
    let (content_type, body) = multipart_body(
        &upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", &fixture_wheel())),
    );
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        post_upload(
            &state,
            "/hosted/",
            Some(&token_basic("__token__", &token)),
            &content_type,
            body,
        )
        .await,
        StatusCode::OK
    );

    drop(guard);
    let text = logs.text();
    assert!(!text.contains(&token));
    assert!(!text.contains("realm-key"));
    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "action") == Some("token_use") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(
        (
            field(&event, "actor"),
            field(&event, "presented_user"),
            field(&event, "token_id")
        ),
        (Some("trusted-publisher:release"), Some(""), Some(token_id.as_str()))
    );
}
#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_delete_policy_denial() {
    let h = harness_with(true, false).await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::FORBIDDEN
    );

    drop(guard);
    let events = logs.security_events();
    let delete = events
        .iter()
        .find(|event| field(event, "action") == Some("delete") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(
        (field(delete, "actor"), field(delete, "presented_user")),
        (Some("uploader"), Some("__token__"))
    );
    assert_eq!(field(delete, "index"), Some("hosted"));
    assert_eq!(field(delete, "hosted_index"), Some("hosted"));
    assert_eq!(field(delete, "resource"), Some("peryxpkg"));
    assert_eq!(
        field(delete, "reason"),
        Some("index is not volatile; delete is disabled")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_the_client_a_trusted_proxy_forwards_for() {
    let harness = proxied_harness().await;
    upload_peryxpkg(&harness.state, "/hosted/", &fixture_wheel()).await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        request_from_peer(
            &harness.state,
            "PUT",
            "/hosted/peryxpkg/1.0/yank",
            Some(&upload_auth()),
            "10.0.0.1",
            "203.0.113.9",
        )
        .await,
        StatusCode::OK
    );

    drop(guard);
    let events = logs.security_events();
    let yank = events
        .iter()
        .find(|event| field(event, "action") == Some("yank") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(yank, "client_ip"), Some("203.0.113.9"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_the_peer_when_an_untrusted_hop_claims_to_forward() {
    let harness = harness_with(true, false).await;
    upload_peryxpkg(&harness.state, "/hosted/", &fixture_wheel()).await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        request_from_peer(
            &harness.state,
            "DELETE",
            "/hosted/peryxpkg/",
            Some(&upload_auth()),
            "198.51.100.7",
            "203.0.113.9",
        )
        .await,
        StatusCode::FORBIDDEN
    );

    drop(guard);
    let events = logs.security_events();
    let delete = events
        .iter()
        .find(|event| field(event, "action") == Some("delete") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(field(delete, "client_ip"), Some("198.51.100.7"));
}

/// Basic credentials authenticate on the password alone, so a client may type any username beside a
/// live secret. The record names the token that authorized the write, and files the name it arrived
/// under where nobody will mistake it for an established identity.
#[tokio::test(flavor = "current_thread")]
async fn test_security_credits_the_matched_token_when_the_username_names_someone_else() {
    let h = harness().await;
    let (content_type, body) = multipart_body(
        &upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", &fixture_wheel())),
    );
    let auth = format!("Basic {}", STANDARD.encode("someone-else:s3cret"));
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        post_upload(&h.state, "/hosted/", Some(&auth), &content_type, body).await,
        StatusCode::OK
    );

    drop(guard);
    let events = logs.security_events();
    let upload = events
        .iter()
        .find(|event| field(event, "action") == Some("upload") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(
        (field(upload, "actor"), field(upload, "presented_user")),
        (Some("uploader"), Some("someone-else"))
    );
}

/// The presented username is client-chosen text of any length, so the record takes a bounded prefix
/// of it and drops the control characters that would forge line and field boundaries in a log.
#[tokio::test(flavor = "current_thread")]
async fn test_security_bounds_the_presented_username() {
    let h = harness().await;
    let (content_type, body) = multipart_body(
        &upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", &fixture_wheel())),
    );
    let presented = format!("{}\r\ninjected", "n".repeat(80));
    let auth = format!("Basic {}", STANDARD.encode(format!("{presented}:s3cret")));
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        post_upload(&h.state, "/hosted/", Some(&auth), &content_type, body).await,
        StatusCode::OK
    );

    drop(guard);
    let events = logs.security_events();
    let upload = events
        .iter()
        .find(|event| field(event, "action") == Some("upload") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(upload, "presented_user"), Some("n".repeat(64).as_str()));
}

/// An anonymous request establishes nobody, and presents nobody either.
#[tokio::test(flavor = "current_thread")]
async fn test_security_records_no_actor_for_an_anonymous_request() {
    let h = harness().await;
    let (content_type, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", b"x")));
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        post_upload(&h.state, "/hosted/", None, &content_type, body).await,
        StatusCode::UNAUTHORIZED
    );

    drop(guard);
    let events = logs.security_events();
    let token = events
        .iter()
        .find(|event| field(event, "action") == Some("token_use") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(
        (field(token, "actor"), field(token, "presented_user")),
        (Some(""), Some(""))
    );
}
