use std::str::FromStr as _;

use rstest::rstest;

use crate::{ArtifactDigest, DigestDecision, RevocationReason, RevocationReasonError};

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn test_artifact_digest_round_trips_canonical_and_in_toto_forms() {
    let digest = ArtifactDigest::from_str(&format!("sha256:{HEX}")).unwrap();

    assert_eq!(digest.sha256(), HEX);
    assert_eq!(digest.canonical(), format!("sha256:{HEX}"));
    assert_eq!(digest.to_string(), format!("sha256:{HEX}"));
    assert_eq!(
        serde_json::to_value(&digest).unwrap(),
        serde_json::json!({"sha256": HEX})
    );
    assert_eq!(
        serde_json::from_value::<ArtifactDigest>(serde_json::json!({"sha256": HEX})).unwrap(),
        digest
    );
}

#[rstest]
#[case::missing_algorithm(HEX)]
#[case::other_algorithm(&format!("sha512:{HEX}"))]
#[case::uppercase(&format!("sha256:{}", HEX.to_uppercase()))]
#[case::short("sha256:deadbeef")]
#[case::non_hex("sha256:z123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")]
fn test_artifact_digest_rejects_noncanonical_text(#[case] value: &str) {
    assert_eq!(
        ArtifactDigest::from_str(value).unwrap_err().to_string(),
        "digest must be sha256:<64 lowercase hexadecimal characters>"
    );
}

#[test]
fn test_artifact_digest_deserialization_rejects_unknown_or_invalid_members() {
    assert!(serde_json::from_value::<ArtifactDigest>(serde_json::json!({"sha256": HEX, "sha512": HEX})).is_err());
    assert!(serde_json::from_value::<ArtifactDigest>(serde_json::json!({"sha256": "bad"})).is_err());
}

#[test]
fn test_revocation_reason_preserves_valid_text_and_round_trips() {
    let reason = RevocationReason::new("  compromised signing host  ").unwrap();

    assert_eq!(reason.as_str(), "  compromised signing host  ");
    assert_eq!(
        serde_json::to_string(&reason).unwrap(),
        "\"  compromised signing host  \""
    );
    assert_eq!(
        serde_json::from_str::<RevocationReason>("\"incident\"")
            .unwrap()
            .as_str(),
        "incident"
    );
}

#[rstest]
#[case::empty("", RevocationReasonError::Empty)]
#[case::whitespace(" \t\n", RevocationReasonError::Empty)]
#[case::too_long(&"a".repeat(2_049), RevocationReasonError::TooLong)]
fn test_revocation_reason_rejects_invalid_text(#[case] value: &str, #[case] expected: RevocationReasonError) {
    assert_eq!(RevocationReason::new(value).unwrap_err(), expected);
    assert!(serde_json::from_value::<RevocationReason>(serde_json::json!(value)).is_err());
}

#[test]
fn test_digest_decisions_are_distinct() {
    assert_ne!(DigestDecision::Clear, DigestDecision::Revoked);
}
