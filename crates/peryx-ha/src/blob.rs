use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    AuthorityEpoch, BlobDurability, BlobMetadata, CommittedMetadata, Digest, JournalCommit, MetadataWriteDurability,
    WriteEvidence,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobAvailabilityFailure {
    Placement,
    Transfer,
    Storage,
}

#[derive(Debug, thiserror::Error)]
#[error("{kind:?}: {source}")]
pub struct BlobAvailabilityError {
    kind: BlobAvailabilityFailure,
    #[source]
    source: Box<dyn Error + Send + Sync>,
}

impl BlobAvailabilityError {
    pub fn new(kind: BlobAvailabilityFailure, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> BlobAvailabilityFailure {
        self.kind
    }
}

#[async_trait]
pub trait BlobAvailability: Send + Sync {
    async fn ensure_local(&self, digest: &Digest) -> Result<Option<BlobMetadata>, BlobAvailabilityError>;
}

#[derive(Debug, Clone, Copy)]
pub struct CommittedBlob<'a> {
    digest: &'a Digest,
    size: u64,
    authority: &'a str,
    epoch: AuthorityEpoch,
    commit: Option<JournalCommit>,
    evidence: WriteEvidence,
}

impl<'a> CommittedBlob<'a> {
    #[must_use]
    pub const fn new(
        digest: &'a Digest,
        size: u64,
        authority: &'a str,
        epoch: AuthorityEpoch,
        commit: Option<JournalCommit>,
        evidence: WriteEvidence,
    ) -> Self {
        Self {
            digest,
            size,
            authority,
            epoch,
            commit,
            evidence,
        }
    }

    #[must_use]
    pub const fn digest(&self) -> &Digest {
        self.digest
    }

    /// The byte length every peer receipt for this write must report.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub const fn authority(&self) -> &str {
        self.authority
    }

    #[must_use]
    pub const fn epoch(&self) -> AuthorityEpoch {
        self.epoch
    }

    #[must_use]
    pub const fn commit(&self) -> Option<JournalCommit> {
        self.commit
    }

    /// What the storage backend proved about these bytes, which decides the class of evidence the write
    /// acknowledgement may count.
    #[must_use]
    pub const fn evidence(&self) -> WriteEvidence {
        self.evidence
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDurability {
    Confirmed { scope: BlobDurability },
    Pending,
    Unavailable,
}

#[async_trait]
pub trait BlobWriteDurability: Send + Sync {
    async fn confirm(&self, write: CommittedBlob<'_>) -> WriteDurability;
}

pub struct BlobServices {
    availability: Option<Arc<dyn BlobAvailability>>,
    durability: Arc<dyn BlobWriteDurability>,
    metadata_durability: Arc<dyn MetadataWriteDurability>,
}

impl BlobServices {
    #[must_use]
    pub fn new(availability: Option<Arc<dyn BlobAvailability>>, durability: Arc<dyn BlobWriteDurability>) -> Self {
        Self {
            availability,
            durability,
            metadata_durability: Arc::new(LocalMetadataDurability),
        }
    }

    #[must_use]
    pub fn with_metadata_durability(mut self, durability: Arc<dyn MetadataWriteDurability>) -> Self {
        self.metadata_durability = durability;
        self
    }

    #[must_use]
    pub fn availability(&self) -> Option<&dyn BlobAvailability> {
        self.availability.as_deref()
    }

    #[must_use]
    pub fn durability(&self) -> &dyn BlobWriteDurability {
        &*self.durability
    }

    #[must_use]
    pub fn metadata_durability(&self) -> &dyn MetadataWriteDurability {
        &*self.metadata_durability
    }
}

struct LocalMetadataDurability;

#[async_trait]
impl MetadataWriteDurability for LocalMetadataDurability {
    async fn confirm_metadata(&self, _write: CommittedMetadata<'_>) -> WriteDurability {
        WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem,
        }
    }
}
