use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rstest::rstest;

use crate::{
    Action, Denial, Glob, Grant, Principal, PublishClaims, PublishDenial, Signer, TrustedPublisher, authorize_grants,
    authorize_publish,
};

const NOW: i64 = 1_000;
const ISSUER: &str = "https://token.actions.githubusercontent.com";
const AUDIENCE: &str = "peryx";

fn publisher() -> TrustedPublisher {
    TrustedPublisher {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        subject: Glob::new("repo:octo/app:*"),
        claims: BTreeMap::from([("repository".to_owned(), "octo/app".to_owned())]),
        projects: vec![Glob::new("app")],
    }
}

fn claims() -> PublishClaims {
    PublishClaims {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        subject: "repo:octo/app:ref:refs/heads/main".to_owned(),
        expires_at: NOW + 300,
        claims: BTreeMap::from([("repository".to_owned(), "octo/app".to_owned())]),
    }
}

#[test]
fn test_matching_identity_earns_a_scoped_write_grant() {
    assert_eq!(
        authorize_publish(&[publisher()], &claims(), NOW),
        Ok(vec![Grant {
            projects: vec![Glob::new("app")],
            actions: BTreeSet::from([Action::Write]),
        }])
    );
}

#[test]
fn test_no_publishers_configured_rejects_the_issuer() {
    assert_eq!(
        authorize_publish(&[], &claims(), NOW),
        Err(PublishDenial::UnknownIssuer)
    );
}

#[rstest]
#[case::wrong_issuer(
    PublishClaims { issuer: "https://gitlab.example/oidc".to_owned(), ..claims() },
    PublishDenial::UnknownIssuer,
)]
#[case::wrong_audience(PublishClaims { audience: "other".to_owned(), ..claims() }, PublishDenial::WrongAudience)]
#[case::expired(PublishClaims { expires_at: NOW, ..claims() }, PublishDenial::Expired)]
#[case::wrong_subject(
    PublishClaims { subject: "repo:octo/other:ref:refs/heads/main".to_owned(), ..claims() },
    PublishDenial::WrongSubject,
)]
#[case::claim_wrong_value(
    PublishClaims { claims: BTreeMap::from([("repository".to_owned(), "octo/fork".to_owned())]), ..claims() },
    PublishDenial::ClaimMismatch { claim: "repository".to_owned() },
)]
#[case::claim_absent(PublishClaims { claims: BTreeMap::new(), ..claims() }, PublishDenial::ClaimMismatch {
    claim: "repository".to_owned(),
})]
fn test_mismatched_identity_is_rejected(#[case] presented: PublishClaims, #[case] expected: PublishDenial) {
    assert_eq!(authorize_publish(&[publisher()], &presented, NOW), Err(expected));
}

#[test]
fn test_publisher_without_extra_claims_matches_on_subject_alone() {
    let open = TrustedPublisher {
        claims: BTreeMap::new(),
        ..publisher()
    };
    assert!(authorize_publish(&[open], &claims(), NOW).is_ok());
}

#[rstest]
#[case::specific_first(vec![wrong_subject_rule(), other_issuer_rule()])]
#[case::specific_last(vec![other_issuer_rule(), wrong_subject_rule()])]
fn test_most_specific_denial_surfaces_regardless_of_order(#[case] publishers: Vec<TrustedPublisher>) {
    assert_eq!(
        authorize_publish(&publishers, &claims(), NOW),
        Err(PublishDenial::WrongSubject)
    );
}

#[test]
fn test_a_later_matching_publisher_still_authorizes() {
    let publishers = vec![other_issuer_rule(), publisher()];
    assert!(authorize_publish(&publishers, &claims(), NOW).is_ok());
}

#[test]
fn test_minted_grant_scopes_uploads_to_the_configured_project() {
    let grants = authorize_publish(&[publisher()], &claims(), NOW).unwrap();
    assert!(authorize_grants(&grants, Some("app"), Action::Write).is_ok());
    assert_eq!(
        authorize_grants(&grants, Some("other"), Action::Write),
        Err(Denial::Forbidden)
    );
    assert_eq!(
        authorize_grants(&grants, Some("app"), Action::Read),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_grant_travels_the_ordinary_token_path() {
    let grants = authorize_publish(&[publisher()], &claims(), NOW).unwrap();
    let signer = Signer::new(b"signing-key", AUDIENCE);
    let subject = Principal::Named {
        subject: claims().subject,
    };
    let issued_at = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap();
    let token = signer.mint(&subject, &grants, issued_at, 300);
    let (recovered, recovered_grants) = signer.verify(&token).unwrap();
    assert_eq!(recovered, subject);
    assert!(authorize_grants(&recovered_grants, Some("app"), Action::Write).is_ok());
}

#[test]
fn test_denials_explain_the_failure() {
    assert_eq!(
        PublishDenial::ClaimMismatch {
            claim: "environment".to_owned(),
        }
        .to_string(),
        "the token is missing the required claim `environment` or carries a different value"
    );
    assert_eq!(PublishDenial::Expired.to_string(), "the token has expired");
}

fn wrong_subject_rule() -> TrustedPublisher {
    TrustedPublisher {
        subject: Glob::new("repo:octo/other:*"),
        ..publisher()
    }
}

fn other_issuer_rule() -> TrustedPublisher {
    TrustedPublisher {
        issuer: "https://gitlab.example/oidc".to_owned(),
        ..publisher()
    }
}
