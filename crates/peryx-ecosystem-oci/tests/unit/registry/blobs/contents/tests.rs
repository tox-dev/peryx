use rstest::rstest;

use super::*;

/// A container build file carries no extension and is not one of the conventional names the generic
/// rules know, so they read it as unknown bytes. The OCI profile claims it as text, because it is the
/// file a user most often wants to read out of a layer.
#[rstest]
#[case::dockerfile("Dockerfile")]
#[case::containerfile("Containerfile")]
#[case::nested("opt/app/Dockerfile")]
fn test_a_build_file_reads_as_text(#[case] path: &str) {
    assert_eq!(PROFILE.member_kind(path), MemberKind::Text);
}

/// The claim above is the profile's own, not something the generic rules would have said anyway.
#[test]
fn test_the_generic_rules_do_not_claim_a_build_file() {
    assert_eq!(generic_member_kind("Dockerfile"), MemberKind::Unknown);
}
