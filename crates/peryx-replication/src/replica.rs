use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use futures_util::StreamExt as _;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use serde::{Deserialize, Serialize};

use crate::error::SyncError;
use crate::protocol::{ChangePage, MetadataMutation, PROTOCOL_VERSION, Primary};

const REPLICA_STATE_KEY: &str = "replication\0state";
const REPLICA_KEY_PREFIX: &str = "replication\0";

/// The durable source identity and last committed serial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaState {
    pub source: String,
    pub serial: u64,
}

/// The result of applying one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    pub changes: usize,
    pub blobs: usize,
    pub serial: u64,
    pub primary_serial: u64,
}

impl SyncOutcome {
    /// Whether the replica reached the primary serial advertised with this page.
    #[must_use]
    pub const fn caught_up(self) -> bool {
        self.serial == self.primary_serial
    }
}

/// A follower that verifies and commits one primary page at a time.
pub struct Replica<'store> {
    meta: &'store MetaStore,
    blobs: &'store BlobStore,
    page_limit: NonZeroUsize,
}

impl<'store> Replica<'store> {
    #[must_use]
    pub const fn new(meta: &'store MetaStore, blobs: &'store BlobStore, page_limit: NonZeroUsize) -> Self {
        Self {
            meta,
            blobs,
            page_limit,
        }
    }

    /// Read the durable resume state.
    ///
    /// # Errors
    /// Returns an error if storage fails, the state cannot be decoded, or its serial differs from
    /// the copied local journal.
    pub fn state(&self) -> Result<Option<ReplicaState>, SyncError> {
        let state = self
            .meta
            .get_driver_value(REPLICA_STATE_KEY)?
            .map(|raw| serde_json::from_slice(&raw))
            .transpose()?;
        let cursor = state.as_ref().map_or(0, |state: &ReplicaState| state.serial);
        let journal = self.meta.current_serial()?;
        if journal != cursor {
            return Err(SyncError::LocalSerialMismatch { cursor, journal });
        }
        Ok(state)
    }

    /// Fetch, verify, and apply the next page after the durable cursor.
    ///
    /// Blob downloads finish before one transaction commits metadata, copied journal entries, and
    /// the next cursor. A failed transfer resumes from the prior serial.
    ///
    /// # Errors
    /// Returns an error for a source failure, invalid page, digest mismatch, or local store failure.
    pub async fn sync_once<P: Primary>(&self, primary: &P) -> Result<SyncOutcome, SyncError> {
        let state = self.state()?;
        let after = state.as_ref().map_or(0, |state| state.serial);
        let page = primary
            .changes(after, self.page_limit.get())
            .await
            .map_err(SyncError::primary)?;
        let validated = ValidatedPage::new(page, after, self.page_limit.get(), state.as_ref())?;
        let mut fetched = 0;
        for (digest, size) in validated.blobs.values() {
            if self.blobs.exists(digest) {
                if !self.blobs.verify(digest)? {
                    return Err(SyncError::CorruptBlob(digest.as_str().to_owned()));
                }
                continue;
            }
            let mut pending = self.blobs.begin()?;
            let mut stream = primary.blob(digest).await.map_err(SyncError::primary)?;
            let mut actual = 0_u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(SyncError::primary)?;
                let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
                if chunk_len > size - actual {
                    return Err(SyncError::BlobSizeMismatch {
                        digest: digest.as_str().to_owned(),
                        expected: *size,
                        actual: actual.saturating_add(chunk_len),
                    });
                }
                actual += chunk_len;
                pending.write(&chunk)?;
            }
            if actual != *size {
                return Err(SyncError::BlobSizeMismatch {
                    digest: digest.as_str().to_owned(),
                    expected: *size,
                    actual,
                });
            }
            self.blobs.commit(pending, digest)?;
            fetched += 1;
        }
        if validated.journal.is_empty() {
            return Ok(SyncOutcome {
                changes: 0,
                blobs: fetched,
                serial: after,
                primary_serial: validated.primary_serial,
            });
        }
        let next_state = serde_json::to_vec(&ReplicaState {
            source: validated.source,
            serial: validated.through,
        })?;
        let changes = validated.journal.len();
        self.meta.commit_replica_txn(after, |txn| {
            for (key, value) in validated.metadata {
                match value {
                    Some(value) => txn.put(&key, &value)?,
                    None => {
                        txn.remove(&key)?;
                    }
                }
            }
            txn.put(REPLICA_STATE_KEY, &next_state)?;
            Ok::<_, SyncError>(((), validated.journal))
        })?;
        Ok(SyncOutcome {
            changes,
            blobs: fetched,
            serial: validated.through,
            primary_serial: validated.primary_serial,
        })
    }
}

struct ValidatedPage {
    source: String,
    through: u64,
    primary_serial: u64,
    journal: Vec<Vec<u8>>,
    metadata: BTreeMap<String, Option<Vec<u8>>>,
    blobs: BTreeMap<String, (Digest, u64)>,
}

impl ValidatedPage {
    fn new(page: ChangePage, after: u64, limit: usize, state: Option<&ReplicaState>) -> Result<Self, SyncError> {
        if page.version != PROTOCOL_VERSION {
            return Err(SyncError::UnsupportedVersion {
                actual: page.version,
                expected: PROTOCOL_VERSION,
            });
        }
        if page.source.is_empty() {
            return Err(SyncError::EmptySource);
        }
        if page.after != after {
            return Err(SyncError::WrongPageStart {
                expected: after,
                actual: page.after,
            });
        }
        if page.changes.len() > limit {
            return Err(SyncError::PageTooLarge {
                limit,
                actual: page.changes.len(),
            });
        }
        if let Some(state) = state.filter(|state| state.source != page.source) {
            return Err(SyncError::SourceChanged {
                expected: state.source.clone(),
                actual: page.source,
            });
        }
        let mut through = after;
        let mut journal = Vec::with_capacity(page.changes.len());
        let mut metadata = BTreeMap::new();
        let mut blobs = BTreeMap::new();
        for change in page.changes {
            if change.serial.checked_sub(1) != Some(through) {
                return Err(SyncError::SerialGap {
                    after: through,
                    actual: change.serial,
                });
            }
            through = change.serial;
            journal.push(change.event);
            for mutation in change.metadata {
                if mutation.key().starts_with(REPLICA_KEY_PREFIX) {
                    return Err(SyncError::ReservedMetadataKey(mutation.key().to_owned()));
                }
                match mutation {
                    MetadataMutation::Put { key, value } => {
                        metadata.insert(key, Some(value));
                    }
                    MetadataMutation::Delete { key } => {
                        metadata.insert(key, None);
                    }
                }
            }
            for blob in change.blobs {
                let digest =
                    Digest::from_hex(&blob.sha256).ok_or_else(|| SyncError::InvalidDigest(blob.sha256.clone()))?;
                if let Some((_, first)) = blobs.insert(blob.sha256.clone(), (digest, blob.size))
                    && first != blob.size
                {
                    return Err(SyncError::ConflictingBlobSize {
                        digest: blob.sha256,
                        first,
                        second: blob.size,
                    });
                }
            }
        }
        if page.current_serial < through {
            return Err(SyncError::PrimaryBehind {
                current: page.current_serial,
                page: through,
            });
        }
        if journal.is_empty() && page.current_serial > after {
            return Err(SyncError::MissingChanges {
                after,
                current: page.current_serial,
            });
        }
        Ok(Self {
            source: page.source,
            through,
            primary_serial: page.current_serial,
            journal,
            metadata,
            blobs,
        })
    }
}
