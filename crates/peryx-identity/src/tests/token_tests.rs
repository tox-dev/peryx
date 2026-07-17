use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;

use crate::{Action, Glob, Grant, Principal, Signer};

const HOUR: i64 = 3600;

fn signer() -> Signer {
    Signer::new(b"signing-key", "peryx")
}

fn grants() -> Vec<Grant> {
    vec![Grant {
        projects: vec![Glob::new("team/*")],
        actions: BTreeSet::from([Action::Write]),
    }]
}

fn named(subject: &str) -> Principal {
    Principal::Named {
        subject: subject.to_owned(),
    }
}

/// `verify` checks expiry against the real clock, so a token minted for a test has to sit on it.
fn now() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
}

fn encoded_with_purpose(purpose: Option<&str>) -> String {
    let mut claims = json!({
        "sub": "ci",
        "aud": "peryx",
        "iat": now(),
        "exp": now() + HOUR,
        "jti": "token-id",
        "grants": grants(),
    });
    if let Some(purpose) = purpose {
        claims["purpose"] = json!(purpose);
    }
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(b"signing-key"),
    )
    .unwrap()
}

#[test]
fn test_mint_and_verify_round_trip_a_named_principal() {
    let signer = signer();
    let token = signer.mint(&named("ci"), &grants(), now(), HOUR);

    assert_eq!(signer.verify(&token).unwrap(), (named("ci"), grants()));
}

#[test]
fn test_mint_and_verify_round_trip_an_anonymous_principal() {
    let signer = signer();
    let token = signer.mint(&Principal::Anonymous, &[], now(), HOUR);

    assert_eq!(signer.verify(&token).unwrap(), (Principal::Anonymous, Vec::new()));
}

#[test]
fn test_verify_rejects_an_expired_token() {
    let signer = signer();
    let token = signer.mint(&named("ci"), &grants(), now() - 2 * HOUR, HOUR);

    assert_eq!(
        signer.verify(&token).unwrap_err().to_string(),
        "invalid token: ExpiredSignature"
    );
}

#[test]
fn test_verify_rejects_a_payload_swapped_under_a_valid_signature() {
    let signer = signer();
    let mine = signer.mint(&named("ci"), &grants(), now(), HOUR);
    let theirs = signer.mint(&named("admin"), &grants(), now(), HOUR);
    let parts: Vec<&str> = mine.split('.').collect();
    let stolen = theirs.split('.').nth(1).unwrap();
    let tampered = format!("{}.{stolen}.{}", parts[0], parts[2]);

    assert_eq!(
        signer.verify(&tampered).unwrap_err().to_string(),
        "invalid token: InvalidSignature"
    );
}

#[test]
fn test_verify_rejects_a_token_another_key_signed() {
    let token = Signer::new(b"other-key", "peryx").mint(&Principal::Anonymous, &[], now(), HOUR);

    assert!(signer().verify(&token).is_err());
}

#[test]
fn test_verify_rejects_a_token_for_another_audience() {
    let token = Signer::new(b"signing-key", "other").mint(&Principal::Anonymous, &[], now(), HOUR);

    assert_eq!(
        signer().verify(&token).unwrap_err().to_string(),
        "invalid token: InvalidAudience"
    );
}

#[test]
fn test_realm_verifier_rejects_a_trusted_publishing_token() {
    let signer = signer();
    let token = signer.mint_trusted(&named("ci"), &grants(), now(), HOUR, "token-id");
    assert!(signer.verify(&token).is_err());
}

#[test]
fn test_trusted_publishing_verifier_rejects_a_realm_token() {
    let signer = signer();
    let token = signer.mint(&named("ci"), &grants(), now(), HOUR);
    assert!(signer.verify_trusted(&token).is_err());
}

#[test]
fn test_absent_purpose_is_compatible_with_the_realm_only() {
    let signer = signer();
    let token = encoded_with_purpose(None);
    assert_eq!(signer.verify(&token).unwrap(), (named("ci"), grants()));
    assert!(signer.verify_trusted(&token).is_err());
}

#[test]
fn test_unknown_purpose_is_rejected_by_every_verifier() {
    let signer = signer();
    let token = encoded_with_purpose(Some("other"));
    assert!(signer.verify(&token).is_err());
    assert!(signer.verify_trusted(&token).is_err());
}
