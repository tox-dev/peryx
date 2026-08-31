//! Availability modes validate a backend's static durability guarantees before serving traffic.
//! Per-operation receipts record the evidence earned by individual writes.

use std::fmt;

use peryx_core::{BlobDurability, DurabilityRequirement, WriteEvidence};

use super::Digest;

/// Evidence that `size` verified bytes crossed the backend's durability boundary at `digest`.
/// Partial, corrupt, or abandoned writes produce no receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementReceipt {
    pub digest: Digest,
    pub size: u64,
    pub durability: DurabilityCapabilities,
    /// What this commit proved, which a later acknowledgement weighs instead of re-deriving evidence
    /// from the backend's configured guarantees.
    pub evidence: WriteEvidence,
}

/// Whether a commit published the bytes at its address or matched them against the object already
/// resident there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    Created,
    VerifiedResident,
}

/// Static guarantees that a backend can provide for completed writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityCapabilities {
    pub domain: BlobDurability,
    /// Readers cannot observe partial uploads.
    pub atomic_publish: bool,
    /// Concurrent writers cannot overwrite committed blobs.
    pub conditional_create: bool,
    /// The backend rejects uploads whose checksum does not match.
    pub checksum_verified: bool,
}

impl DurabilityCapabilities {
    /// Guarantees provided by the local filesystem backend.
    pub const FILESYSTEM: Self = Self {
        domain: BlobDurability::Filesystem,
        atomic_publish: true,
        conditional_create: true,
        checksum_verified: true,
    };

    #[must_use]
    pub const fn object_store(conditional_create: bool, checksum_verified: bool) -> Self {
        Self {
            domain: BlobDurability::ObjectStore,
            atomic_publish: true,
            conditional_create,
            checksum_verified,
        }
    }

    /// The evidence an object-store commit earns.
    ///
    /// A store speaks for a write it published only under both guarantees: without the precondition the
    /// service may have overwritten another writer, and without the validated checksum it never
    /// confirmed the bytes it stored are the ones addressed. A commit that lost the precondition and
    /// then read the resident object back needs neither, because the read measured the bytes at that
    /// address directly.
    #[must_use]
    pub const fn object_store_evidence(self, publication: Publication) -> WriteEvidence {
        match publication {
            Publication::VerifiedResident => WriteEvidence::ObjectStoreVerified,
            Publication::Created if self.conditional_create && self.checksum_verified => {
                WriteEvidence::ObjectStoreVerified
            }
            Publication::Created => WriteEvidence::ObjectStoreUnverified,
        }
    }

    /// Checks guarantees in protocol-defined order.
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

/// [`fmt::Display`] never exposes an endpoint, bucket, path, or credential.
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
#[path = "../../tests/unit/blob/durability/tests.rs"]
mod tests;
