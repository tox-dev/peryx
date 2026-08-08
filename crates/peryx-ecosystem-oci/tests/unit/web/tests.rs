use rstest::rstest;

use super::{manifest_from_bytes, members_from_bytes, pull_command};
use crate::name::Reference;

#[test]
fn test_members_from_bytes_parses_a_listing() {
    let members =
        members_from_bytes(br#"{"members":[{"path":"a.txt","size":3,"kind":"text","previewable":true}]}"#).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].path, "a.txt");
}

#[rstest]
#[case::tag(Reference::Tag("latest".to_owned()), "docker pull <host>/team/app:latest")]
#[case::digest(
    Reference::Digest("sha256:abc".to_owned()),
    "docker pull <host>/team/app@sha256:abc"
)]
fn test_pull_command_uses_the_reference_separator(#[case] reference: Reference, #[case] expected: &str) {
    assert_eq!(pull_command("team/app", &reference), expected);
}

#[test]
fn test_members_from_bytes_rejects_invalid_json() {
    assert!(members_from_bytes(b"not json").is_err());
}

#[test]
fn test_manifest_from_bytes_rejects_invalid_json() {
    assert!(manifest_from_bytes(b"not json").is_err());
}

#[rstest]
#[case::image(br#"{"config":{"size":10},"layers":[{"size":3},{"size":4}]}"#, false, 17)]
#[case::index(br#"{"manifests":[{"size":5},{"size":6}]}"#, true, 11)]
#[case::image_saturates(
    br#"{"config":{"size":18446744073709551615},"layers":[{"size":1}]}"#,
    false,
    u64::MAX
)]
#[case::index_saturates(
    br#"{"manifests":[{"size":18446744073709551615},{"size":18446744073709551615}]}"#,
    true,
    u64::MAX
)]
fn test_manifest_from_bytes_totals_sizes_and_saturates_overflow(
    #[case] bytes: &[u8],
    #[case] is_index: bool,
    #[case] total_size: u64,
) {
    let manifest = manifest_from_bytes(bytes).unwrap();
    assert_eq!((manifest.is_index, manifest.total_size), (is_index, total_size));
}
