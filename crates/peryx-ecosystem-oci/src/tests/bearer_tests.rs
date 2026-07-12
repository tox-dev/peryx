//! The OCI Bearer token realm end to end: the `/v2/` challenge, the `/v2/token` endpoint, and the
//! scoped enforcement on resource routes that together make `docker login` validate.

use axum::http::{Method, StatusCode, header};
use rstest::rstest;

use peryx_identity::Action;

use super::{
    auth, body_has_code, hosted_writable, oci_digest, realm_app, scoped_index, send, send_body, send_with, token_from,
    writable_index,
};

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const SECRET: &str = "s3cret";

/// A hosted index that gates every read behind a token whose `team/*` grant reads and writes.
fn team_registry(dir: &tempfile::TempDir) -> axum::Router {
    let index = scoped_index("store", "store", "ci", SECRET, "team/*", &[Action::Read, Action::Write]);
    let (_state, app) = realm_app(dir, vec![index]);
    app
}

/// Request a token, returning the status and the minted JWT (empty when the request was refused).
async fn request_token(app: &axum::Router, query: &str, authorization: Option<&str>) -> (StatusCode, String) {
    let headers: Vec<(&str, &str)> = authorization
        .map(|value| vec![("authorization", value)])
        .unwrap_or_default();
    let (status, _, body) = send_with(app, Method::GET, &format!("/v2/token?{query}"), &headers).await;
    let token = if status == StatusCode::OK {
        token_from(&body)
    } else {
        String::new()
    };
    (status, token)
}

#[tokio::test]
async fn test_v2_challenges_with_a_bearer_realm_when_an_acl_restricts() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (status, headers, _) = send_with(&app, Method::GET, "/v2/", &[("host", "registry.example:5000")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        "Bearer realm=\"http://registry.example:5000/v2/token\",service=\"peryx\""
    );
}

#[tokio::test]
async fn test_v2_stays_open_for_an_anonymous_deployment() {
    let dir = tempfile::tempdir().unwrap();
    // A signer is installed, but no index restricts access, so the frictionless default holds.
    let (_state, app) = realm_app(
        &dir,
        vec![super::oci_index(
            "store",
            "store",
            super::IndexKind::Hosted { volatile: true },
        )],
    );
    let (status, headers, _) = send(&app, Method::GET, "/v2/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-distribution-api-version"], "registry/2.0");
}

#[tokio::test]
async fn test_v2_accepts_a_valid_basic_credential() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (status, _, _) = send_with(&app, Method::GET, "/v2/", &[("authorization", &auth(SECRET))]).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_token_endpoint_issues_an_anonymous_token_that_pulls_a_public_repo() {
    let dir = tempfile::tempdir().unwrap();
    // `writable_index` reads anonymously (a public repo) but still carries a credential, so the realm
    // challenges and issues tokens.
    let (_state, app) = realm_app(&dir, vec![writable_index("pub", "pub", true, SECRET)]);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    push(&app, "pub/app", &digest, body, &auth(SECRET)).await;

    let (status, token) = request_token(&app, "service=peryx&scope=repository:pub/app:pull", None).await;
    assert_eq!(status, StatusCode::OK);

    let (pull, _, got) = send_with(
        &app,
        Method::GET,
        &format!("/v2/pub/app/manifests/{digest}"),
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await;
    assert_eq!(pull, StatusCode::OK);
    assert_eq!(got, &body[..]);
}

#[tokio::test]
async fn test_named_token_pushes_within_its_glob_and_is_refused_outside_it() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);

    // A token scoped to the repository its glob covers pushes there.
    let (_, granted) = request_token(
        &app,
        "service=peryx&scope=repository:store/team/app:pull,push",
        Some(&auth(SECRET)),
    )
    .await;
    push(&app, "store/team/app", &digest, body, &format!("Bearer {granted}")).await;

    // A token minted for a repository the glob does not cover carries no access, so presenting it is a
    // valid-but-insufficient credential the registry names in the challenge.
    let (_, denied_token) = request_token(
        &app,
        "service=peryx&scope=repository:store/other/app:pull,push",
        Some(&auth(SECRET)),
    )
    .await;
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/other/app/manifests/{digest}"),
        &[
            ("authorization", &format!("Bearer {denied_token}")),
            ("content-type", MANIFEST_TYPE),
        ],
        body.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        "Bearer realm=\"/v2/token\",service=\"peryx\",scope=\"repository:store/other/app:pull,push\",error=\"insufficient_scope\""
    );
}

#[rstest]
#[case::repository("repository:store/team/app:pull,push", "store/team/other")]
#[case::action("repository:store/team/app:pull", "store/team/app")]
#[tokio::test]
async fn test_bearer_token_may_not_push_beyond_scope(#[case] requested_scope: &str, #[case] target: &str) {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (_, token) = request_token(
        &app,
        &format!("service=peryx&scope={requested_scope}"),
        Some(&auth(SECRET)),
    )
    .await;
    let body = br#"{"schemaVersion":2}"#;
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/{target}/manifests/{}", oci_digest(body)),
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", MANIFEST_TYPE),
        ],
        body.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        format!(
            r#"Bearer realm="/v2/token",service="peryx",scope="repository:{target}:pull,push",error="insufficient_scope""#
        )
    );
}

#[rstest]
#[case::different_secret("other", StatusCode::UNAUTHORIZED)]
#[case::same_secret(SECRET, StatusCode::OK)]
#[tokio::test]
async fn test_named_subject_isolated_by_index_secret(#[case] target_secret: &str, #[case] expected: StatusCode) {
    let dir = tempfile::tempdir().unwrap();
    let actions = &[Action::Read, Action::Write];
    let indexes = vec![
        scoped_index("source", "source", "ci", SECRET, "*", actions),
        scoped_index("target", "target", "ci", target_secret, "*", actions),
    ];
    let (_state, app) = realm_app(&dir, indexes);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    push(&app, "target/app", &digest, body, &auth(target_secret)).await;
    let (_, token) = request_token(
        &app,
        "service=peryx&scope=repository:target/app:pull",
        Some(&auth(SECRET)),
    )
    .await;

    let (status, _, _) = send_with(
        &app,
        Method::GET,
        &format!("/v2/target/app/manifests/{digest}"),
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await;
    assert_eq!(status, expected);
}

#[tokio::test]
async fn test_docker_login_flow_validates_and_then_pushes() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);

    // 1. The pull-nothing probe learns the realm.
    let (probe, ..) = send(&app, Method::GET, "/v2/").await;
    assert_eq!(probe, StatusCode::UNAUTHORIZED);

    // 2. `docker login` requests a token with the Basic credentials and no scope; a valid password gets
    //    a token even though it carries no grants.
    let (login, token) = request_token(&app, "service=peryx", Some(&auth(SECRET))).await;
    assert_eq!(login, StatusCode::OK);

    // 3. The probe with that token confirms the credentials.
    let (confirm, ..) = send_with(
        &app,
        Method::GET,
        "/v2/",
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await;
    assert_eq!(confirm, StatusCode::OK);

    // 4. A scoped token then authorizes the push.
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    let (_, scoped) = request_token(
        &app,
        "service=peryx&scope=repository:store/team/app:pull,push",
        Some(&auth(SECRET)),
    )
    .await;
    push(&app, "store/team/app", &digest, body, &format!("Bearer {scoped}")).await;
}

#[tokio::test]
async fn test_an_invalid_bearer_is_named_invalid_token() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (status, headers, _) = send_with(
        &app,
        Method::GET,
        "/v2/store/team/app/manifests/latest",
        &[("authorization", "Bearer not-a-real-token")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        "Bearer realm=\"/v2/token\",service=\"peryx\",scope=\"repository:store/team/app:pull\",error=\"invalid_token\""
    );
}

#[tokio::test]
async fn test_a_gated_read_without_credentials_is_challenged_for_its_scope() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    // No credential on a repository whose reads are not anonymous: a plain challenge naming the pull
    // scope, with no `error` — the client has not failed, it simply has not authenticated yet.
    let (status, headers, _) = send(&app, Method::GET, "/v2/store/team/app/manifests/latest").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        "Bearer realm=\"/v2/token\",service=\"peryx\",scope=\"repository:store/team/app:pull\""
    );
}

#[tokio::test]
async fn test_basic_is_still_accepted_on_a_resource_push() {
    let dir = tempfile::tempdir().unwrap();
    // A realm is configured, yet the legacy `docker login -u _ -p <token>` push over Basic keeps working.
    let (_state, app) = realm_app(&dir, vec![writable_index("store", "store", true, SECRET)]);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    push(&app, "store/app", &digest, body, &auth(SECRET)).await;
}

#[tokio::test]
async fn test_token_endpoint_rejects_a_wrong_password() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (status, _) = request_token(&app, "service=peryx", Some(&auth("wrong"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[rstest]
#[case::absent("")]
#[case::different("service=other")]
#[case::duplicated("service=peryx&service=peryx")]
#[tokio::test]
async fn test_token_endpoint_rejects_an_invalid_service(#[case] query: &str) {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (status, _) = request_token(&app, query, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_token_endpoint_ignores_an_unresolvable_scope() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    // A scope naming no configured index resolves to nothing; the token is still issued, just empty.
    let (status, _) = request_token(
        &app,
        "service=peryx&scope=repository:ghost/app:pull",
        Some(&auth(SECRET)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_token_endpoint_treats_a_non_basic_header_as_anonymous() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    // A header that is not Basic is no login attempt, so the endpoint issues an anonymous token rather
    // than a `401`.
    let (status, _) = request_token(&app, "service=peryx", Some("Bearer whatever")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_token_endpoint_accepts_a_trailing_slash() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (status, _, _) = send(&app, Method::GET, "/v2/token/?service=peryx").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_token_endpoint_is_unsupported_without_a_realm() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, SECRET);
    let (status, _, body) = send(&app, Method::GET, "/v2/token?service=peryx").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(body_has_code(&body, "UNSUPPORTED"), "{body:?}");
}

// A credential the realm does not accept leaves `GET /v2/` challenging: a bearer it did not sign, a
// Basic password that authenticates on no index, and a scheme it does not speak at all.
#[rstest]
#[case::forged_bearer("Bearer forged".to_owned())]
#[case::wrong_basic(auth("wrong"))]
#[case::unknown_scheme("Digest deadbeef".to_owned())]
#[tokio::test]
async fn test_v2_challenges_an_invalid_credential(#[case] authorization: String) {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    let (status, _, _) = send_with(&app, Method::GET, "/v2/", &[("authorization", &authorization)]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_read_of_an_unresolvable_name_is_name_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let app = team_registry(&dir);
    // A well-formed name under no configured index route resolves to nothing, so the read gate passes
    // through and the manifest handler answers name-unknown.
    let (status, _, body) = send(&app, Method::GET, "/v2/ghost/app/manifests/latest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "NAME_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_push_with_an_invalid_bearer_is_named_invalid_token() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = realm_app(&dir, vec![writable_index("store", "store", true, SECRET)]);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", "Bearer forged")],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        "Bearer realm=\"/v2/token\",service=\"peryx\",scope=\"repository:store/app:pull,push\",error=\"invalid_token\""
    );
}

#[tokio::test]
async fn test_bearer_on_a_realmless_index_falls_back_to_basic() {
    let dir = tempfile::tempdir().unwrap();
    // With no signing key, a bearer cannot be verified, so it is ignored and the write falls back to the
    // Basic challenge a realm-less registry answers.
    let (_state, app) = hosted_writable(&dir, SECRET);
    let (status, headers, _) = send_with(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", "Bearer ignored-without-a-realm")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx\"");
}

/// Push a manifest by digest with the given `Authorization`, asserting it is created.
async fn push(app: &axum::Router, name: &str, digest: &str, body: &[u8], authorization: &str) {
    let (status, _, response) = send_body(
        app,
        Method::PUT,
        &format!("/v2/{name}/manifests/{digest}"),
        &[("authorization", authorization), ("content-type", MANIFEST_TYPE)],
        body.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{response:?}");
}
