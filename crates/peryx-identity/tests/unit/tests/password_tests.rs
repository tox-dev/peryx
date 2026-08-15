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

#[test]
fn test_check_reports_stale_when_the_policy_tightens() {
    let verifier = cheap().hash("correct horse").unwrap();
    let tighter = PasswordPolicy::new(16, 2, 1).unwrap();

    assert_eq!(
        verifier.check("correct horse", &tighter),
        PasswordCheck::Accepted { stale: true }
    );
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
