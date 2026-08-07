//! Typed durability guarantees a blob backend proves, and the requirement an availability mode
//! places on them.
//!
//! Static claims resolve once at startup: a mode that acknowledges writes across nodes must not
//! treat a bare storage success as proof, so it checks the selected backend's guarantees before it
//! serves traffic. Per-operation results still carry the dynamic evidence a completed write earned;
//! this module only models what the backend can promise in advance.

use std::fmt;

use super::{BlobDurability, Digest};

/// The durability evidence one completed write earned: the per-operation counterpart to the static
/// [`DurabilityCapabilities`] a backend advertises in advance.
///
/// A filesystem backend mints one only after the write crosses its durability boundary - the bytes are
/// hashed, the file is synced, its temporary file is atomically renamed into the content-addressed
/// path, and the parent directory is synced - so holding a receipt is proof that `size` bytes are
/// durably stored and served under `digest`. A partial, corrupt, or abandoned write never yields one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementReceipt {
    /// The content address the durable bytes are served under.
    pub digest: Digest,
    /// The durable byte length.
    pub size: u64,
    /// The evidence this write earned within its failure domain.
    pub durability: DurabilityCapabilities,
}

/// What a configured backend proves about a completed write.
///
/// [`BlobDurability`] names the failure domain the write survives; the flags describe the atomic-write
/// evidence a success carries within that domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityCapabilities {
    /// The failure domain an acknowledged write survives.
    pub domain: BlobDurability,
    /// A blob becomes readable only once fully written; a partial upload is never visible.
    pub atomic_publish: bool,
    /// Create-if-absent is enforced by the store, so a concurrent writer cannot silently overwrite a
    /// committed blob.
    pub conditional_create: bool,
    /// The store validates the content checksum on write, so a corrupted upload is rejected rather
    /// than acknowledged.
    pub checksum_verified: bool,
}

impl DurabilityCapabilities {
    /// A local filesystem backend: single-host durability behind an atomic rename that refuses to
    /// clobber an existing blob and commits only bytes that hash to the expected digest.
    pub const FILESYSTEM: Self = Self {
        domain: BlobDurability::Filesystem,
        atomic_publish: true,
        conditional_create: true,
        checksum_verified: true,
    };

    /// An object-store backend whose endpoint honors both conditional writes and checksum validation.
    #[must_use]
    pub const fn object_store(conditional_create: bool, checksum_verified: bool) -> Self {
        Self {
            domain: BlobDurability::ObjectStore,
            atomic_publish: true,
            conditional_create,
            checksum_verified,
        }
    }

    /// The first guarantee `requirement` asks for that this backend cannot prove.
    ///
    /// # Errors
    /// Returns the missing guarantee when the backend falls short of the requirement.
    pub const fn check(self, requirement: DurabilityRequirement) -> Result<(), DurabilityShortfall> {
        if requirement.conditional_create && !self.conditional_create {
            return Err(DurabilityShortfall::ConditionalCreate);
        }
        if requirement.checksum_verified && !self.checksum_verified {
            return Err(DurabilityShortfall::ChecksumVerified);
        }
        Ok(())
    }
}

/// What an availability mode requires before a write may be acknowledged as durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityRequirement {
    pub conditional_create: bool,
    pub checksum_verified: bool,
}

impl DurabilityRequirement {
    /// A single-node process acknowledges from local durability alone, so it asks for no store-level
    /// coordination evidence.
    pub const LOCAL: Self = Self {
        conditional_create: false,
        checksum_verified: false,
    };

    /// A replicated mode acknowledges only from race-safe, integrity-checked writes, so a bare
    /// success on one node cannot stand in for proof.
    pub const REPLICATED: Self = Self {
        conditional_create: true,
        checksum_verified: true,
    };
}

/// The guarantee a backend cannot prove for the selected mode.
///
/// The [`fmt::Display`] rendering names only the missing guarantee: it never carries an endpoint,
/// bucket, path, or credential, so it is safe on an operator-facing startup error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityShortfall {
    ConditionalCreate,
    ChecksumVerified,
}

impl DurabilityShortfall {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConditionalCreate => "conditional create-if-absent writes",
            Self::ChecksumVerified => "checksum-validated writes",
        }
    }
}

impl fmt::Display for DurabilityShortfall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "the configured blob backend cannot prove {}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
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
}
