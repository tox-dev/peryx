use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use peryx_identity::{Action, OidcTokenVerifier, Principal, VerifiedOidcIdentity};
use rstest::rstest;

use super::*;

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
        repository: "private".to_owned(),
        publisher: TrustedPublisher {
            issuer: "https://issuer.example".to_owned(),
            audience: "peryx".to_owned(),
            subject: Glob::new("repo:org/app:*"),
            claims: BTreeMap::from([("repository_id".to_owned(), "42".to_owned())]),
            projects: vec![Glob::new("app")],
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
    )
    .unwrap();
    (signer, runtime)
}

#[test]
fn test_runtime_reports_configured_audience() {
    let mut binding = binding();
    binding.publisher.audience = "packages.example".to_owned();
    let (_, runtime) = runtime(vec![binding], MAX_REPLAY_ENTRIES);

    assert_eq!(runtime.audience(), "packages.example");
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
            "private",
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
async fn test_empty_repository_keeps_project_grants_unqualified() {
    let mut unqualified = binding();
    unqualified.repository.clear();
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
#[case::parent_repository(vec![PublisherBinding { repository: "../private".to_owned(), ..binding() }], 300, MAX_REPLAY_ENTRIES)]
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
