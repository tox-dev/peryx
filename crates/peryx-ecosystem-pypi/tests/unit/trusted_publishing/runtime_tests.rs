use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peryx_driver::serving::IndexedProtocolDriver as _;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_identity::{Action, IndexAcl, OidcTokenVerifier, Principal, VerifiedOidcIdentity};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use sigstore_verify::Verifier as SigstoreVerifier;
use sigstore_verify::trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, TrustedRoot};
use tower::ServiceExt as _;

use super::*;
use crate::trusted_publishing::policy::AttestationPolicy;

const NOW: i64 = 2_000_000_000;

struct Verifier {
    identity: VerifiedOidcIdentity,
    error: Option<OidcVerificationError>,
}

#[async_trait]
impl OidcTokenVerifier for Verifier {
    async fn verify(
        &self,
        token: &str,
        expected_audience: &str,
        _now: i64,
    ) -> Result<VerifiedOidcIdentity, OidcVerificationError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if expected_audience != self.identity.audience {
            return Err(OidcVerificationError::InvalidIdentity);
        }
        Ok(VerifiedOidcIdentity {
            token_id: token.to_owned(),
            ..self.identity.clone()
        })
    }
}

fn binding() -> PublisherBinding {
    PublisherBinding {
        id: "github-release".to_owned(),
        repository: "root-pypi".to_owned(),
        route: "private".to_owned(),
        publisher: TrustedPublisher {
            issuer: "https://issuer.example".to_owned(),
            audience: "peryx".to_owned(),
            subject: Glob::new("repo:org/app:*"),
            claims: BTreeMap::from([("repository_id".to_owned(), "42".to_owned())]),
            projects: vec![Glob::new("app")],
            attestation: None,
        },
    }
}

fn identity() -> VerifiedOidcIdentity {
    VerifiedOidcIdentity {
        issuer: "https://issuer.example".to_owned(),
        audience: "peryx".to_owned(),
        subject: "repo:org/app:ref:refs/heads/main".to_owned(),
        expires_at: NOW + 600,
        token_id: String::new(),
        claims: BTreeMap::from([("repository_id".to_owned(), serde_json::json!("42"))]),
    }
}

fn runtime(bindings: Vec<PublisherBinding>, replay_capacity: usize) -> (Signer, OidcRuntime) {
    let signer = Signer::new(b"local-key", "peryx");
    let runtime = OidcRuntime::build(
        bindings,
        Arc::new(Verifier {
            identity: identity(),
            error: None,
        }),
        signer.clone(),
        300,
        replay_capacity,
        None,
    )
    .unwrap();
    (signer, runtime)
}

#[tokio::test]
async fn test_exchange_caps_the_minted_ttl_at_the_identitys_remaining_lifetime() {
    let signer = Signer::new(b"local-key", "peryx");
    let runtime = OidcRuntime::build(
        vec![binding()],
        Arc::new(Verifier {
            identity: VerifiedOidcIdentity {
                expires_at: NOW + 100,
                ..identity()
            },
            error: None,
        }),
        signer,
        300,
        MAX_REPLAY_ENTRIES,
        None,
    )
    .unwrap();

    let exchanged = runtime.exchange("external-short-lived", NOW).await.unwrap();

    assert_eq!(exchanged.expires_at, NOW + 100);
}

#[test]
fn test_consume_replay_purges_an_entry_at_the_moment_it_expires() {
    let (_signer, runtime) = runtime(vec![binding()], MAX_REPLAY_ENTRIES);
    runtime.consume_replay("issuer", "token-1", 100, 0).unwrap();

    assert!(
        runtime.consume_replay("issuer", "token-1", 200, 100).is_ok(),
        "an entry whose expiry equals `now` has already expired"
    );
}

#[test]
fn test_runtime_reports_configured_audience() {
    let mut binding = binding();
    binding.publisher.audience = "packages.example".to_owned();
    let (_, runtime) = runtime(vec![binding], MAX_REPLAY_ENTRIES);

    assert_eq!(runtime.audience(), "packages.example");
}

#[rstest]
#[case::anonymous(Principal::Anonymous, false)]
#[case::publisher(
    Principal::Named {
        subject: "trusted-publisher:github-release".to_owned(),
    },
    true
)]
fn test_attestation_context_requires_a_named_configured_publisher(
    #[case] principal: Principal,
    #[case] expected: bool,
) {
    let mut binding = binding();
    binding.publisher.attestation = Some(AttestationPolicy {
        identity: "https://identity.example".to_owned(),
        claims: BTreeMap::new(),
    });
    let root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT).unwrap();
    let runtime = OidcRuntime::build(
        vec![binding],
        Arc::new(Verifier {
            identity: identity(),
            error: None,
        }),
        Signer::new(b"local-key", "peryx"),
        300,
        MAX_REPLAY_ENTRIES,
        Some(Arc::new(SigstoreVerifier::new(&root).unwrap())),
    )
    .unwrap();
    let token = VerifiedToken {
        principal,
        grants: Vec::new(),
        id: "token".to_owned(),
    };

    assert_eq!(runtime.attestation_context(&token).is_some(), expected);
}

#[tokio::test]
async fn test_exchange_mints_a_repository_scoped_token_once() {
    let (signer, runtime) = runtime(vec![binding()], MAX_REPLAY_ENTRIES);
    let exchanged = runtime.exchange("external-1", NOW).await.unwrap();
    let internal = signer.verify_scoped(&exchanged.token, TOKEN_SCOPE).unwrap();

    assert_eq!(
        (
            exchanged.publisher_id.as_str(),
            internal.id.as_str(),
            exchanged.token_id.as_str(),
            exchanged.repository.as_str(),
            exchanged.expires_at,
            internal.principal,
        ),
        (
            "github-release",
            internal.id.as_str(),
            internal.id.as_str(),
            "root-pypi",
            NOW + 300,
            Principal::Named {
                subject: "trusted-publisher:github-release".to_owned(),
            },
        )
    );
    assert!(
        peryx_identity::authorize_grants(
            &internal.grants,
            peryx_identity::ResourceMatch::Pattern("private/app"),
            Action::Write
        )
        .is_ok()
    );
    assert!(
        peryx_identity::authorize_grants(
            &internal.grants,
            peryx_identity::ResourceMatch::Pattern("other/app"),
            Action::Write
        )
        .is_err()
    );
    assert!(matches!(
        runtime.exchange("external-1", NOW).await,
        Err(ExchangeError::Replay)
    ));
}

#[rstest]
#[case::named("root/pypi", "/root/pypi/")]
#[case::root("", "/")]
#[tokio::test]
async fn test_minted_token_uploads_through_the_configured_index_route(#[case] route: &str, #[case] upload_uri: &str) {
    let (_directory, state) = publishing_state(route);
    let token = mint_token(&state).await;

    assert_eq!(upload(&state, route, upload_uri, &token).await, StatusCode::OK);
}

#[rstest]
#[case::sibling("/sibling/")]
#[case::hosted_layer("/internal/")]
#[tokio::test]
async fn test_minted_token_cannot_upload_outside_the_configured_index_route(#[case] upload_uri: &str) {
    let (_directory, state) = publishing_state("root/pypi");
    let token = mint_token(&state).await;

    assert_eq!(
        upload(&state, "root/pypi", upload_uri, &token).await,
        StatusCode::FORBIDDEN
    );
}

async fn upload(state: &Arc<AppState>, route: &str, upload_uri: &str, token: &str) -> StatusCode {
    let (content_type, body) = crate::tests::http::multipart_body(
        &crate::tests::http::upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", &crate::tests::http::fixture_wheel())),
    );
    if route.is_empty() {
        return crate::PypiServing
            .post(
                Arc::clone(&state.serving),
                String::new(),
                Request::builder()
                    .method(Method::POST)
                    .uri(upload_uri)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .status();
    }
    crate::tests::http::post_upload(state, upload_uri, Some(&format!("Bearer {token}")), &content_type, body).await
}

async fn mint_token(state: &Arc<AppState>) -> String {
    let response = peryx_http::router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/_/oidc/mint-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"token":"external"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn publishing_state(route: &str) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        vec![
            Index {
                name: "hosted".to_owned(),
                route: "internal".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: true },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "root-pypi".to_owned(),
                route: route.to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: Some(0),
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "sibling".to_owned(),
                route: "sibling".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: Some(0),
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
        ],
    );
    crate::tests::install(&mut state);
    let mut publisher = binding();
    publisher.route = route.to_owned();
    publisher.publisher.projects = vec![Glob::new("peryxpkg")];
    let runtime = Arc::new(runtime(vec![publisher], MAX_REPLAY_ENTRIES).1);
    {
        let mut context = state.auth_install_context().unwrap();
        context.register_service(Arc::clone(&runtime));
        context.register_routes(Arc::new(super::super::http::TrustedPublishingRoutes::new(runtime)));
    }
    (directory, Arc::new(state))
}

#[tokio::test]
async fn test_concurrent_exchange_has_one_winner() {
    let (_, runtime) = runtime(vec![binding()], MAX_REPLAY_ENTRIES);
    let (first, second) = tokio::join!(runtime.exchange("race", NOW), runtime.exchange("race", NOW));

    assert_eq!(
        (
            usize::from(first.is_ok()) + usize::from(second.is_ok()),
            usize::from(matches!(first, Err(ExchangeError::Replay)))
                + usize::from(matches!(second, Err(ExchangeError::Replay))),
        ),
        (1, 1)
    );
}

#[tokio::test]
async fn test_replay_capacity_rejects_a_distinct_identity() {
    let (_, runtime) = runtime(vec![binding()], 1);
    runtime.exchange("first", NOW).await.unwrap();
    assert!(matches!(
        runtime.exchange("second", NOW).await,
        Err(ExchangeError::ReplayCapacity)
    ));
}

#[tokio::test]
async fn test_empty_route_keeps_project_grants_unqualified() {
    let mut unqualified = binding();
    unqualified.route.clear();
    let (signer, runtime) = runtime(vec![unqualified], MAX_REPLAY_ENTRIES);
    let exchanged = runtime.exchange("external", NOW).await.unwrap();

    assert_eq!(
        signer.verify_scoped(&exchanged.token, TOKEN_SCOPE).unwrap().grants[0].resources,
        vec![Glob::new("app")]
    );
}

#[tokio::test]
async fn test_verification_error_is_preserved() {
    let runtime = OidcRuntime::build(
        vec![binding()],
        Arc::new(Verifier {
            identity: identity(),
            error: Some(OidcVerificationError::IssuerUnavailable),
        }),
        Signer::new(b"local-key", "peryx"),
        300,
        MAX_REPLAY_ENTRIES,
        None,
    )
    .unwrap();

    assert!(matches!(
        runtime.exchange("external", NOW).await,
        Err(ExchangeError::Verification(OidcVerificationError::IssuerUnavailable))
    ));
}

#[rstest]
#[case::empty(Vec::new(), 300, MAX_REPLAY_ENTRIES)]
#[case::ttl(vec![binding()], 0, MAX_REPLAY_ENTRIES)]
#[case::capacity(vec![binding()], 300, 0)]
#[case::empty_id(vec![PublisherBinding { id: String::new(), ..binding() }], 300, MAX_REPLAY_ENTRIES)]
#[case::parent_route(vec![PublisherBinding { route: "../private".to_owned(), ..binding() }], 300, MAX_REPLAY_ENTRIES)]
#[case::duplicate_id(vec![binding(), binding()], 300, MAX_REPLAY_ENTRIES)]
#[case::mixed_audience(vec![binding(), PublisherBinding {
    id: "other".to_owned(),
    publisher: TrustedPublisher { audience: "other".to_owned(), ..binding().publisher },
    ..binding()
}], 300, MAX_REPLAY_ENTRIES)]
fn test_build_rejects_invalid_configuration(
    #[case] bindings: Vec<PublisherBinding>,
    #[case] ttl_secs: i64,
    #[case] replay_capacity: usize,
) {
    assert!(matches!(
        OidcRuntime::build(
            bindings,
            Arc::new(Verifier {
                identity: identity(),
                error: None,
            }),
            Signer::new(b"local-key", "peryx"),
            ttl_secs,
            replay_capacity,
            None,
        ),
        Err(ExchangeError::Configuration)
    ));
}

#[rstest]
#[case::issuer(OidcVerificationError::IssuerUnavailable, true)]
#[case::response(OidcVerificationError::InvalidIssuerResponse, true)]
#[case::key(OidcVerificationError::UnknownKey, true)]
#[case::identity(OidcVerificationError::InvalidIdentity, false)]
fn test_exchange_error_availability(#[case] verification: OidcVerificationError, #[case] expected: bool) {
    assert_eq!(ExchangeError::Verification(verification).unavailable(), expected);
}

#[test]
fn test_nonverification_error_availability() {
    assert!(ExchangeError::ReplayCapacity.unavailable());
    assert!(!ExchangeError::Configuration.unavailable());
    assert!(!ExchangeError::Replay.unavailable());
    assert!(!ExchangeError::Denied(PublishDenial::UnknownIssuer).unavailable());
}

#[test]
fn test_token_scope_spelling_is_stable() {
    assert_eq!(TOKEN_SCOPE.as_str(), "trusted-publishing");
}
