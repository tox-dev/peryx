use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use peryx_identity::ArtifactDigest;
use peryx_storage::blob::Digest;
use peryx_storage::meta::{DriverBlobReference, DriverMutation, JournalEntry, MetaStore, ServerMutation};
use serde::{Deserialize, Serialize};

use crate::error::SyncError;
use crate::protocol::{ChangePage, MetadataMutation, PROTOCOL_VERSION, Primary};

const REPLICA_STATE_KEY: &str = "replication\0state";
const REPLICA_KEY_PREFIX: &str = "replication\0";

/// Durable resume cursor pinned to one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaState {
    pub source: String,
    pub serial: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    pub changes: usize,
    pub serial: u64,
    pub primary_serial: u64,
}

impl SyncOutcome {
    #[must_use]
    pub const fn caught_up(self) -> bool {
        self.serial == self.primary_serial
    }
}

/// Verifies and commits each primary page in one transaction.
pub struct Replica<'store> {
    meta: &'store MetaStore,
    page_limit: NonZeroUsize,
}

/// What one applied page changed locally, for the views that have to catch up with it.
pub struct AppliedPage {
    pub outcome: SyncOutcome,
    /// Driver rows the page wrote or removed.
    pub changed_keys: Vec<String>,
    /// `(digest, size)` pairs the page's entries reference, for the blob plane to fetch.
    pub referenced: Vec<(Digest, u64)>,
    /// Digests whose revocation row the page rewrote, whose cached serving decisions are now stale.
    pub revocations: Vec<ArtifactDigest>,
}

impl<'store> Replica<'store> {
    #[must_use]
    pub const fn new(meta: &'store MetaStore, page_limit: NonZeroUsize) -> Self {
        Self { meta, page_limit }
    }

    /// Reads the resume cursor and verifies that it matches the local journal.
    ///
    /// # Errors
    /// Returns an error if storage fails, decoding fails, or the cursor differs from the journal serial.
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

    /// Commits metadata, journal entries, cursor, and blob references in one transaction. The blob plane
    /// fetches bytes later and keeps their serial outside the readable frontier until they arrive.
    ///
    /// # Errors
    /// Returns an error for a source failure, invalid page, or local store failure.
    pub async fn sync<P: Primary>(&self, primary: &P) -> Result<AppliedPage, SyncError> {
        let after = self.state()?.as_ref().map_or(0, |state| state.serial);
        let page = primary
            .changes(after, self.page_limit.get())
            .await
            .map_err(SyncError::primary)?;
        self.apply_page(page)
    }

    /// Uses the same validation and transaction boundary for every page transport.
    ///
    /// # Errors
    /// Returns [`SyncError`] when validation or the commit transaction fails.
    pub fn apply_page(&self, page: ChangePage) -> Result<AppliedPage, SyncError> {
        let state = self.state()?;
        let after = state.as_ref().map_or(0, |state| state.serial);
        let ValidatedPage {
            source,
            through,
            primary_serial,
            changes,
            changed_keys,
            referenced,
        } = ValidatedPage::new(page, after, self.page_limit.get(), state.as_ref())?;
        let referenced = referenced.into_values().collect();
        if changes.is_empty() {
            return Ok(AppliedPage {
                outcome: SyncOutcome {
                    changes: 0,
                    serial: after,
                    primary_serial,
                },
                changed_keys: Vec::new(),
                referenced,
                revocations: Vec::new(),
            });
        }
        let next_state = serde_json::to_vec(&ReplicaState {
            source,
            serial: through,
        })?;
        let change_count = changes.len();
        let mut revocations = Vec::new();
        self.meta.commit_replica_txn(after, |txn| {
            let mut journal = Vec::with_capacity(changes.len());
            for change in changes {
                let mut mutations = Vec::with_capacity(change.metadata.len());
                for mutation in change.metadata {
                    match mutation {
                        MetadataMutation::Put { key, value } => {
                            txn.put(&key, &value)?;
                            mutations.push(DriverMutation::Put { key, value });
                        }
                        MetadataMutation::Delete { key } => {
                            txn.remove(&key)?;
                            mutations.push(DriverMutation::Delete { key });
                        }
                    }
                }
                if let Some(server) = change.server {
                    let ServerMutation::DigestRevocation { record } = &server;
                    revocations.push(record.digest.clone());
                    txn.apply_server_mutation(&server)?;
                }
                journal.push(JournalEntry {
                    payload: change.event,
                    mutations,
                    blobs: change
                        .blobs
                        .into_iter()
                        .map(|(digest, size)| DriverBlobReference {
                            sha256: digest.as_str().to_owned(),
                            size,
                        })
                        .collect(),
                });
            }
            txn.put_local(REPLICA_STATE_KEY, &next_state)?;
            Ok::<_, SyncError>(((), journal))
        })?;
        Ok(AppliedPage {
            outcome: SyncOutcome {
                changes: change_count,
                serial: through,
                primary_serial,
            },
            changed_keys: changed_keys.into_iter().collect(),
            referenced,
            revocations,
        })
    }
}

struct ValidatedPage {
    source: String,
    through: u64,
    primary_serial: u64,
    changes: Vec<ValidatedChange>,
    changed_keys: BTreeSet<String>,
    referenced: BTreeMap<String, (Digest, u64)>,
}

struct ValidatedChange {
    event: Vec<u8>,
    /// The core change the entry carries, decoded before the commit so a payload that claims a core
    /// operation it does not describe rejects the page instead of silently skipping the change.
    server: Option<ServerMutation>,
    metadata: Vec<MetadataMutation>,
    blobs: Vec<(Digest, u64)>,
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
        let mut changes = Vec::with_capacity(page.changes.len());
        let mut changed_keys = BTreeSet::new();
        let mut referenced = BTreeMap::new();
        for change in page.changes {
            if change.serial.checked_sub(1) != Some(through) {
                return Err(SyncError::SerialGap {
                    after: through,
                    actual: change.serial,
                });
            }
            through = change.serial;
            for mutation in &change.metadata {
                if mutation.key().starts_with(REPLICA_KEY_PREFIX) {
                    return Err(SyncError::ReservedMetadataKey(mutation.key().to_owned()));
                }
                changed_keys.insert(mutation.key().to_owned());
            }
            let mut blobs = Vec::with_capacity(change.blobs.len());
            for blob in change.blobs {
                let digest =
                    Digest::from_hex(&blob.sha256).ok_or_else(|| SyncError::InvalidDigest(blob.sha256.clone()))?;
                if let Some((_, first)) = referenced.insert(blob.sha256.clone(), (digest.clone(), blob.size))
                    && first != blob.size
                {
                    return Err(SyncError::ConflictingBlobSize {
                        digest: blob.sha256,
                        first,
                        second: blob.size,
                    });
                }
                blobs.push((digest, blob.size));
            }
            changes.push(ValidatedChange {
                server: ServerMutation::decode(&change.event)?,
                event: change.event,
                metadata: change.metadata,
                blobs,
            });
        }
        if page.current_serial < through {
            return Err(SyncError::PrimaryBehind {
                current: page.current_serial,
                page: through,
            });
        }
        if changes.is_empty() && page.current_serial > after {
            return Err(SyncError::MissingChanges {
                after,
                current: page.current_serial,
            });
        }
        Ok(Self {
            source: page.source,
            through,
            primary_serial: page.current_serial,
            changes,
            changed_keys,
            referenced,
        })
    }
}
