use super::*;

#[test]
fn blob_durability_names_each_scope() {
    assert_eq!(
        [
            BlobDurability::Filesystem.as_str(),
            BlobDurability::ObjectStore.as_str()
        ],
        ["filesystem", "object-store"]
    );
}

#[test]
fn digest_hashes_and_parses_canonical_sha256() {
    let digest = Digest::of(b"peryx");

    assert_eq!(Digest::from_hex(digest.as_str()), Some(digest));
}

#[test]
fn digest_rejects_noncanonical_sha256() {
    assert_eq!(
        [Digest::from_hex("abc"), Digest::from_hex(&"G".repeat(64))],
        [None, None]
    );
}

#[test]
fn durability_requirements_preserve_mode_guarantees() {
    assert_eq!(
        [DurabilityRequirement::LOCAL, DurabilityRequirement::REPLICATED],
        [
            DurabilityRequirement {
                conditional_create: false,
                checksum_verified: false,
            },
            DurabilityRequirement {
                conditional_create: true,
                checksum_verified: true,
            },
        ]
    );
}

#[test]
fn journal_commit_exposes_its_serial() {
    assert_eq!(JournalCommit::new(17).serial(), 17);
}

#[test]
fn observed_frontier_requires_each_configured_plane() {
    assert_eq!(
        [
            ObservedFrontier {
                replica: None,
                backup: None,
            }
            .covers(5),
            ObservedFrontier {
                replica: Some(5),
                backup: Some(6),
            }
            .covers(5),
            ObservedFrontier {
                replica: Some(4),
                backup: Some(6),
            }
            .covers(5),
            ObservedFrontier {
                replica: Some(6),
                backup: Some(4),
            }
            .covers(5),
        ],
        [true, true, false, false]
    );
}

#[test]
fn availability_read_error_preserves_backend_message() {
    assert_eq!(AvailabilityReadError::new("read failed").to_string(), "read failed");
}
