use rstest::rstest;

use super::{DurabilityCapabilities, DurabilityRequirement, DurabilityShortfall, Publication};
use crate::blob::{BlobDurability, WriteEvidence};

#[test]
fn test_filesystem_proves_every_guarantee() {
    assert_eq!(
        DurabilityCapabilities::FILESYSTEM,
        DurabilityCapabilities {
            domain: BlobDurability::Filesystem,
            atomic_publish: true,
            conditional_create: true,
            checksum_verified: true,
        }
    );
    assert_eq!(
        DurabilityCapabilities::FILESYSTEM.check(DurabilityRequirement::REPLICATED),
        Ok(())
    );
}

#[test]
fn test_object_store_reports_its_domain_and_evidence() {
    assert_eq!(
        DurabilityCapabilities::object_store(true, false),
        DurabilityCapabilities {
            domain: BlobDurability::ObjectStore,
            atomic_publish: true,
            conditional_create: true,
            checksum_verified: false,
        }
    );
}

#[test]
fn test_local_requirement_accepts_a_backend_with_no_coordination_evidence() {
    let caps = DurabilityCapabilities::object_store(false, false);
    assert_eq!(caps.check(DurabilityRequirement::LOCAL), Ok(()));
}

#[rstest]
#[case::missing_conditional(false, true, DurabilityShortfall::ConditionalCreate)]
#[case::missing_checksum(true, false, DurabilityShortfall::ChecksumVerified)]
fn test_replicated_requirement_reports_the_missing_guarantee(
    #[case] conditional_create: bool,
    #[case] checksum_verified: bool,
    #[case] expected: DurabilityShortfall,
) {
    let caps = DurabilityCapabilities::object_store(conditional_create, checksum_verified);
    assert_eq!(caps.check(DurabilityRequirement::REPLICATED), Err(expected));
}

#[rstest]
#[case::conditional(DurabilityShortfall::ConditionalCreate, "conditional create-if-absent writes")]
#[case::checksum(DurabilityShortfall::ChecksumVerified, "checksum-validated writes")]
fn test_shortfall_names_only_the_guarantee(#[case] shortfall: DurabilityShortfall, #[case] guarantee: &str) {
    assert_eq!(shortfall.as_str(), guarantee);
    let rendered = shortfall.to_string();
    assert!(rendered.contains(guarantee));
    assert!(!rendered.contains("bucket"));
    assert!(!rendered.contains("http"));
}

#[rstest]
#[case::published(true, true, Publication::Created, WriteEvidence::ObjectStoreVerified)]
#[case::unconditional(false, true, Publication::Created, WriteEvidence::ObjectStoreUnverified)]
#[case::unchecksummed(true, false, Publication::Created, WriteEvidence::ObjectStoreUnverified)]
// A read of the resident object measured the bytes itself, so the write's own guarantees add nothing.
#[case::verified_resident(false, false, Publication::VerifiedResident, WriteEvidence::ObjectStoreVerified)]
fn test_only_a_guarded_publication_earns_object_store_evidence(
    #[case] conditional_create: bool,
    #[case] checksum_verified: bool,
    #[case] publication: Publication,
    #[case] expected: WriteEvidence,
) {
    let caps = DurabilityCapabilities::object_store(conditional_create, checksum_verified);

    assert_eq!(caps.object_store_evidence(publication), expected);
}

#[rstest]
#[case::node_local(WriteEvidence::NodeLocal, BlobDurability::Filesystem)]
#[case::published(WriteEvidence::ObjectStoreVerified, BlobDurability::ObjectStore)]
#[case::occupied(WriteEvidence::ObjectStoreUnverified, BlobDurability::ObjectStore)]
fn test_evidence_names_the_durability_domain_it_was_earned_in(
    #[case] evidence: WriteEvidence,
    #[case] expected: BlobDurability,
) {
    assert_eq!(evidence.scope(), expected);
}
