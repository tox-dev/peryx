//! Security event logging at the mutation points: a denial and a success are both logged, and never
//! with the credential that was presented.

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
            && field(event, "actor") == Some("__token__")
            && field(event, "index") == Some("hosted")
    }));
    let upload = events
        .iter()
        .find(|event| field(event, "action") == Some("upload") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(upload, "index"), Some("root/pypi"));
    assert_eq!(field(upload, "hosted_index"), Some("hosted"));
    assert_eq!(field(upload, "project"), Some("peryxpkg"));
    assert_eq!(field(upload, "version"), Some("1.0"));
    assert_eq!(field(upload, "filename"), Some("peryxpkg-1.0-py3-none-any.whl"));
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
    assert_eq!(field(token, "actor"), Some("alice"));
    assert_eq!(field(token, "index"), Some("hosted"));
    assert_eq!(field(token, "reason"), Some("invalid upload token"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_trusted_token_id_without_the_token() {
    let (_dir, state, signer) = trusted_publishing();
    let token = trusted_token(&signer, "hosted/peryxpkg");
    let token_id = signer.verify_trusted(&token).unwrap().id;
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
        (field(&event, "actor"), field(&event, "token_id")),
        (Some("trusted-publisher:release"), Some(token_id.as_str()))
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
    assert_eq!(field(delete, "actor"), Some("__token__"));
    assert_eq!(field(delete, "index"), Some("hosted"));
    assert_eq!(field(delete, "hosted_index"), Some("hosted"));
    assert_eq!(field(delete, "project"), Some("peryxpkg"));
    assert_eq!(
        field(delete, "reason"),
        Some("index is not volatile; delete is disabled")
    );
}
