//! Availability modes validate a backend's static durability guarantees before serving traffic.
//! Per-operation receipts record the evidence earned by individual writes.

use std::fmt;

use peryx_core::{BlobDurability, DurabilityRequirement};

use super::Digest;

/// Evidence that `size` verified bytes crossed the backend's durability boundary at `digest`.
/// Partial, corrupt, or abandoned writes produce no receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementReceipt {
    pub digest: Digest,
    pub size: u64,
    pub durability: DurabilityCapabilities,
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
