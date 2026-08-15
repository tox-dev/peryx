use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;

use crate::{Action, Glob, Grant, Principal, Signer, TokenScope};

const HOUR: i64 = 3600;
const VALID_ISSUED_AT: i64 = 4_102_441_200;
const EXTENSION_SCOPE: TokenScope = TokenScope::new("extension");

/// Bypass expiry validation to inspect the signed boundary value.
fn signed_exp(token: &str) -> i64 {
    let payload = token.split('.').nth(1).unwrap();
    let bytes = URL_SAFE_NO_PAD.decode(payload).unwrap();
    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["exp"]
        .as_i64()
        .unwrap()
}

fn signer() -> Signer {
    Signer::new(b"signing-key", "peryx")
}

fn grants() -> Vec<Grant> {
    vec![Grant {
        resources: vec![Glob::new("team/*")],
        actions: BTreeSet::from([Action::Write]),
    }]
}

fn named(subject: &str) -> Principal {
    Principal::Named {
        subject: subject.to_owned(),
    }
}

fn encoded_with_purpose(purpose: Option<&str>) -> String {
    let mut claims = json!({
        "sub": "ci",
        "aud": "peryx",
        "iat": VALID_ISSUED_AT,
        "exp": VALID_ISSUED_AT + HOUR,
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
    let token = signer.mint(&named("ci"), &grants(), VALID_ISSUED_AT, HOUR);

    assert_eq!(signer.verify(&token).unwrap(), (named("ci"), grants()));
}

#[test]
fn test_signer_reports_its_audience() {
    assert_eq!(signer().audience(), "peryx");
}

#[test]
fn test_token_scope_preserves_its_name() {
    let scope = TokenScope::new(std::hint::black_box("extension"));
    assert_eq!(scope.as_str(), "extension");
}

#[test]
fn test_mint_and_verify_round_trip_an_anonymous_principal() {
    let signer = signer();
    let token = signer.mint(&Principal::Anonymous, &[], VALID_ISSUED_AT, HOUR);

    assert_eq!(signer.verify(&token).unwrap(), (Principal::Anonymous, Vec::new()));
}

#[test]
fn test_verify_rejects_an_expired_token() {
    let signer = signer();
    let token = signer.mint(&named("ci"), &grants(), 1, HOUR);

    assert_eq!(
        signer.verify(&token).unwrap_err().to_string(),
        "invalid token: ExpiredSignature"
    );
}

#[test]
fn test_mint_sets_exp_to_the_sum_of_iat_and_ttl() {
    let token = signer().mint(&named("ci"), &grants(), 1_000, HOUR);

    assert_eq!(signed_exp(&token), 1_000 + HOUR);
}

#[test]
fn test_mint_clamps_exp_instead_of_wrapping_past_the_i64_boundary() {
    let token = signer().mint(&named("ci"), &grants(), i64::MAX - 1, HOUR);

    assert_eq!(signed_exp(&token), i64::MAX);
}

#[test]
fn test_verify_rejects_a_payload_swapped_under_a_valid_signature() {
    let signer = signer();
    let mine = signer.mint(&named("ci"), &grants(), VALID_ISSUED_AT, HOUR);
    let theirs = signer.mint(&named("admin"), &grants(), VALID_ISSUED_AT, HOUR);
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
    let token = Signer::new(b"other-key", "peryx").mint(&Principal::Anonymous, &[], VALID_ISSUED_AT, HOUR);

    assert!(signer().verify(&token).is_err());
}

#[test]
fn test_verify_rejects_a_token_for_another_audience() {
    let token = Signer::new(b"signing-key", "other").mint(&Principal::Anonymous, &[], VALID_ISSUED_AT, HOUR);

    assert_eq!(
        signer().verify(&token).unwrap_err().to_string(),
        "invalid token: InvalidAudience"
    );
}

#[test]
fn test_realm_verifier_rejects_an_extension_token() {
    let signer = signer();
    let token = signer.mint_scoped(
        EXTENSION_SCOPE,
        &named("ci"),
        &grants(),
        VALID_ISSUED_AT,
        HOUR,
        "token-id",
    );
    assert!(signer.verify(&token).is_err());
}

#[test]
fn test_extension_verifier_rejects_a_realm_token() {
    let signer = signer();
    let token = signer.mint(&named("ci"), &grants(), VALID_ISSUED_AT, HOUR);
    assert!(signer.verify_scoped(&token, EXTENSION_SCOPE).is_err());
}

#[test]
fn test_absent_purpose_is_compatible_with_the_realm_only() {
    let signer = signer();
    let token = encoded_with_purpose(None);
    assert_eq!(signer.verify(&token).unwrap(), (named("ci"), grants()));
    assert!(signer.verify_scoped(&token, EXTENSION_SCOPE).is_err());
}

#[test]
fn test_unknown_purpose_is_rejected_by_every_verifier() {
    let signer = signer();
    let token = encoded_with_purpose(Some("other"));
    assert!(signer.verify(&token).is_err());
    assert!(signer.verify_scoped(&token, EXTENSION_SCOPE).is_err());
}
