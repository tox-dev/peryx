use async_trait::async_trait;

use crate::{AuthorityEpoch, JournalCommit, OperationKind, WriteDurability};

#[derive(Debug, Clone, Copy)]
pub struct CommittedMetadata<'a> {
    authority: &'a str,
    epoch: AuthorityEpoch,
    commit: JournalCommit,
    kind: OperationKind,
}

impl<'a> CommittedMetadata<'a> {
    #[must_use]
    pub const fn new(authority: &'a str, epoch: AuthorityEpoch, commit: JournalCommit, kind: OperationKind) -> Self {
        Self {
            authority,
            epoch,
            commit,
            kind,
        }
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
    pub const fn commit(&self) -> JournalCommit {
        self.commit
    }

    /// The mutation class the write belongs to, which its trace records.
    #[must_use]
    pub const fn kind(&self) -> OperationKind {
        self.kind
    }
}

#[async_trait]
pub trait MetadataWriteDurability: Send + Sync {
    async fn confirm_metadata(&self, write: CommittedMetadata<'_>) -> WriteDurability;
}
