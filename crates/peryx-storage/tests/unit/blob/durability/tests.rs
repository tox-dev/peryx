use rstest::rstest;

use super::{DurabilityCapabilities, DurabilityRequirement, DurabilityShortfall};
use crate::blob::BlobDurability;

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
