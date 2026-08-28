use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rstest::rstest;

use crate::{PasswordCheck, PasswordError, PasswordPolicy, PasswordVerifier};

fn cheap() -> PasswordPolicy {
    PasswordPolicy::new(8, 1, 1).unwrap()
}

#[test]
fn test_hash_then_check_accepts_the_enrolled_password() {
    let verifier = cheap().hash("correct horse").unwrap();

    assert_eq!(
        verifier.check("correct horse", &cheap()),
        PasswordCheck::Accepted { stale: false }
    );
}

#[test]
fn test_check_rejects_a_wrong_password() {
    let verifier = cheap().hash("correct horse").unwrap();

    assert_eq!(verifier.check("battery staple", &cheap()), PasswordCheck::Rejected);
}

#[test]
fn test_each_enrollment_uses_a_fresh_salt() {
    let policy = cheap();

    assert_ne!(policy.hash("same").unwrap(), policy.hash("same").unwrap());
}

#[rstest]
#[case::memory(24, 1, 2)]
#[case::iterations(16, 2, 2)]
#[case::lanes(16, 1, 1)]
fn test_check_reports_each_stale_cost(#[case] memory_kib: u32, #[case] iterations: u32, #[case] lanes: u32) {
    let verifier = PasswordPolicy::new(16, 1, 2).unwrap().hash("correct horse").unwrap();
    let changed = PasswordPolicy::new(memory_kib, iterations, lanes).unwrap();

    assert_eq!(
        verifier.check("correct horse", &changed),
        PasswordCheck::Accepted { stale: true }
    );
}

#[rstest]
#[case::argon2i(Algorithm::Argon2i, Version::V0x13, Params::DEFAULT_OUTPUT_LEN)]
#[case::argon2d(Algorithm::Argon2d, Version::V0x13, Params::DEFAULT_OUTPUT_LEN)]
#[case::version_16(Algorithm::Argon2id, Version::V0x10, Params::DEFAULT_OUTPUT_LEN)]
#[case::short_output(Algorithm::Argon2id, Version::V0x13, 16)]
fn test_check_reports_each_stale_profile(
    #[case] algorithm: Algorithm,
    #[case] version: Version,
    #[case] output_len: usize,
) {
    let verifier = custom_verifier(algorithm, version, output_len);

    assert_eq!(
        verifier.check("correct horse", &cheap()),
        PasswordCheck::Accepted { stale: true }
    );
}

#[test]
fn test_check_rejects_a_wrong_password_for_a_legacy_profile() {
    let verifier = custom_verifier(Algorithm::Argon2i, Version::V0x10, 16);

    assert_eq!(verifier.check("battery staple", &cheap()), PasswordCheck::Rejected);
}

#[test]
fn test_check_rejects_a_malformed_verifier() {
    let verifier: PasswordVerifier = serde_json::from_str("\"not-a-phc-string\"").unwrap();

    assert_eq!(verifier.check("anything", &cheap()), PasswordCheck::Rejected);
}

#[test]
fn test_new_rejects_costs_below_the_argon2_floor() {
    assert_eq!(PasswordPolicy::new(1, 1, 1), Err(PasswordError::Params));
}

#[test]
fn test_recommended_policy_round_trips_a_password() {
    let policy = PasswordPolicy::recommended();
    let verifier = policy.hash("correct horse").unwrap();

    assert_eq!(
        verifier.check("correct horse", &policy),
        PasswordCheck::Accepted { stale: false }
    );
}

#[test]
fn test_spend_decoy_runs_without_a_stored_verifier() {
    cheap().spend_decoy("guess");
}

#[test]
fn test_debug_redacts_the_verifier() {
    let verifier = cheap().hash("correct horse").unwrap();

    assert_eq!(format!("{verifier:?}"), "PasswordVerifier(<redacted>)");
}

fn custom_verifier(algorithm: Algorithm, version: Version, output_len: usize) -> PasswordVerifier {
    let salt = SaltString::encode_b64(&[0; 16]).unwrap();
    let params = Params::new(8, 1, 1, Some(output_len)).unwrap();
    let encoded = Argon2::new(algorithm, version, params)
        .hash_password(b"correct horse", &salt)
        .unwrap()
        .to_string();
    serde_json::from_value(serde_json::Value::String(encoded)).unwrap()
}
