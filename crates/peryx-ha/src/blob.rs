use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;

use crate::{AuthorityEpoch, BlobDurability, BlobMetadata, Digest, JournalCommit};

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
    authority: &'a str,
    epoch: AuthorityEpoch,
    commit: Option<JournalCommit>,
    local_durability: BlobDurability,
}

impl<'a> CommittedBlob<'a> {
    #[must_use]
    pub const fn new(
        digest: &'a Digest,
        authority: &'a str,
        epoch: AuthorityEpoch,
        commit: Option<JournalCommit>,
        local_durability: BlobDurability,
    ) -> Self {
        Self {
            digest,
            authority,
            epoch,
            commit,
            local_durability,
        }
    }

    #[must_use]
    pub const fn digest(&self) -> &Digest {
        self.digest
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

    #[must_use]
    pub const fn local_durability(&self) -> BlobDurability {
        self.local_durability
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
}

impl BlobServices {
    #[must_use]
    pub fn new(availability: Option<Arc<dyn BlobAvailability>>, durability: Arc<dyn BlobWriteDurability>) -> Self {
        Self {
            availability,
            durability,
        }
    }

    #[must_use]
    pub fn availability(&self) -> Option<&dyn BlobAvailability> {
        self.availability.as_deref()
    }

    #[must_use]
    pub fn durability(&self) -> &dyn BlobWriteDurability {
        &*self.durability
    }
}
