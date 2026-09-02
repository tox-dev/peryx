//! A checkpoint is the replicated state folded out of the journal.
//!
//! Replicated state is not a set of tables that could be dumped. [`DRIVER_KV`] holds replicated and
//! local rows under the same keys, because [`DriverTxn::upsert`](super::DriverTxn::upsert) and
//! [`upsert_local`](super::DriverTxn::upsert_local) write the same table with the same bytes and
//! differ only in whether the key joins the transaction's replicated mutation set. Nothing at rest
//! records which is which, so the only exact account of what replicated is the journal itself.
//!
//! Folding it is therefore the definition rather than an optimisation: the replicated state at
//! serial `C` is what a replica that applied every record through `C` would hold. A fold over the
//! previous checkpoint plus the records after it equals a fold from empty through `C`, which is what
//! lets a later checkpoint read only the tail. The same primitive serves a replica folding its own
//! journal over its own last install, so it takes a base and a range rather than assuming a writer.
//!
//! Nothing here removes a journal row or advances any floor. Pruning stays unsafe until a replica
//! that falls behind the retained history can fetch and install one of these.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Included};

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::error::MetaError;
use super::journal::{DriverBlobReference, DriverMutation};
use super::revocation::DigestRevocation;
use super::server_mutation::ServerMutation;
use super::{
    CHECKPOINT_BLOB, CHECKPOINT_META, CHECKPOINT_REVOCATION, CHECKPOINT_ROW, JOURNAL, JOURNAL_BLOBS, JOURNAL_MUTATIONS,
    MetaStore, open_optional_table,
};

/// Names the single manifest row, which one publication replaces whole.
const MANIFEST_KEY: &str = "manifest";

/// What names a checkpoint's origin. The versions belong to the replication layer rather than to
/// storage, so the caller supplies them and storage only binds them into the manifest it signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointIdentity {
    pub source: String,
    pub protocol_version: u16,
    pub schema_version: u32,
}

/// The published pointer a consumer verifies before it trusts a checkpoint.
///
/// The counts and `bytes` are part of what is signed, so a truncated transfer fails the comparison
/// before its digest is ever recomputed over the wrong length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub identity: CheckpointIdentity,
    pub serial: u64,
    pub rows: u64,
    pub revocations: u64,
    pub blobs: u64,
    /// Canonical encoding length, which sizes a later chunked transfer.
    pub bytes: u64,
    /// Hex SHA-256 over the canonical encoding of the folded state.
    pub digest: String,
}

/// The replicated state at one serial, in the order its digest covers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckpointState {
    rows: BTreeMap<String, Vec<u8>>,
    revocations: BTreeMap<String, DigestRevocation>,
    blobs: BTreeSet<DriverBlobReference>,
}

/// A manifest and the state it publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub manifest: CheckpointManifest,
    pub state: CheckpointState,
}

/// Why a checkpoint cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointVerifyError {
    #[error("checkpoint declares {declared} {unit} and carries {actual}")]
    Truncated {
        unit: &'static str,
        declared: u64,
        actual: u64,
    },
    #[error("checkpoint digest is {actual}, and its manifest declares {declared}")]
    Digest { declared: String, actual: String },
}

impl CheckpointState {
    #[must_use]
    pub const fn rows(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.rows
    }

    #[must_use]
    pub const fn revocations(&self) -> &BTreeMap<String, DigestRevocation> {
        &self.revocations
    }

    #[must_use]
    pub const fn blobs(&self) -> &BTreeSet<DriverBlobReference> {
        &self.blobs
    }

    /// Rebuilds a state a consumer received, so a transfer can be verified before it is installed.
    #[must_use]
    pub const fn from_parts(
        rows: BTreeMap<String, Vec<u8>>,
        revocations: BTreeMap<String, DigestRevocation>,
        blobs: BTreeSet<DriverBlobReference>,
    ) -> Self {
        Self {
            rows,
            revocations,
            blobs,
        }
    }

    /// Folds one journal record in, in the order the writer committed them.
    ///
    /// A payload that carries no core operation belongs to an ecosystem driver and contributes only
    /// through the mutations beside it.
    ///
    /// # Errors
    /// Returns a decode error when a payload claims a core operation it does not describe.
    pub fn apply(
        &mut self,
        mutations: Vec<DriverMutation>,
        blobs: Vec<DriverBlobReference>,
        payload: &[u8],
    ) -> Result<(), MetaError> {
        for mutation in mutations {
            match mutation {
                DriverMutation::Put { key, value } => {
                    self.rows.insert(key, value);
                }
                DriverMutation::Delete { key } => {
                    self.rows.remove(&key);
                }
            }
        }
        if let Some(ServerMutation::DigestRevocation { record }) = ServerMutation::decode(payload)? {
            self.revocations.insert(record.digest.canonical(), record);
        }
        self.blobs.extend(blobs);
        Ok(())
    }

    /// The bytes a manifest's digest and length cover.
    ///
    /// Every field is length-prefixed, so no arrangement of keys and values can encode the way a
    /// different arrangement does.
    ///
    /// # Panics
    /// Panics if a revocation does not serialize, which its field types cannot refuse.
    #[must_use]
    pub fn canonical(&self) -> Vec<u8> {
        let mut encoded = Vec::new();
        for (key, value) in &self.rows {
            push_field(&mut encoded, b'r', key.as_bytes());
            push_bytes(&mut encoded, value);
        }
        for (digest, record) in &self.revocations {
            push_field(&mut encoded, b'v', digest.as_bytes());
            push_bytes(
                &mut encoded,
                &serde_json::to_vec(record).expect("a stored revocation always serializes to JSON"),
            );
        }
        for blob in &self.blobs {
            push_field(&mut encoded, b'b', blob.sha256.as_bytes());
            encoded.extend_from_slice(&blob.size.to_le_bytes());
        }
        encoded
    }

    /// Names this state at `serial`. A consumer builds the same manifest from what it received and
    /// compares it with the one it was given.
    #[must_use]
    pub fn manifest(&self, identity: CheckpointIdentity, serial: u64) -> CheckpointManifest {
        let canonical = self.canonical();
        CheckpointManifest {
            identity,
            serial,
            rows: self.rows.len() as u64,
            revocations: self.revocations.len() as u64,
            blobs: self.blobs.len() as u64,
            bytes: canonical.len() as u64,
            digest: hex::encode(Sha256::digest(&canonical)),
        }
    }
}

fn push_field(encoded: &mut Vec<u8>, tag: u8, key: &[u8]) {
    encoded.push(tag);
    push_bytes(encoded, key);
}

fn push_bytes(encoded: &mut Vec<u8>, bytes: &[u8]) {
    encoded.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    encoded.extend_from_slice(bytes);
}

impl Checkpoint {
    /// Checks what a consumer received against what the manifest declares.
    ///
    /// # Errors
    /// Returns [`CheckpointVerifyError`] when a count or the digest disagrees.
    pub fn verify(&self) -> Result<(), CheckpointVerifyError> {
        let canonical = self.state.canonical();
        for (unit, declared, actual) in [
            ("rows", self.manifest.rows, self.state.rows.len() as u64),
            (
                "revocations",
                self.manifest.revocations,
                self.state.revocations.len() as u64,
            ),
            ("blobs", self.manifest.blobs, self.state.blobs.len() as u64),
            ("bytes", self.manifest.bytes, canonical.len() as u64),
        ] {
            if declared != actual {
                return Err(CheckpointVerifyError::Truncated { unit, declared, actual });
            }
        }
        let actual = hex::encode(Sha256::digest(&canonical));
        if actual != self.manifest.digest {
            return Err(CheckpointVerifyError::Digest {
                declared: self.manifest.digest.clone(),
                actual,
            });
        }
        Ok(())
    }
}

impl MetaStore {
    /// Folds the journal after `base` through `through` onto `state`.
    ///
    /// Takes the base state rather than reading it, so a replica can fold its own journal over its
    /// own last install through the same path the writer uses.
    ///
    /// # Errors
    /// Returns a store error if the read fails, or a decode error for a malformed record.
    pub fn fold_journal(&self, state: &mut CheckpointState, base: u64, through: u64) -> Result<(), MetaError> {
        let txn = self.db.begin_read()?;
        let Some(journal) = open_optional_table(&txn, JOURNAL)? else {
            return Ok(());
        };
        let mutations = open_optional_table(&txn, JOURNAL_MUTATIONS)?;
        let blobs = open_optional_table(&txn, JOURNAL_BLOBS)?;
        for entry in journal.range((Excluded(base), Included(through)))? {
            let (serial, payload) = entry?;
            let serial = serial.value();
            let mutations = mutations
                .as_ref()
                .and_then(|table| table.get(serial).transpose())
                .transpose()?
                .map(|value| serde_json::from_slice(value.value()))
                .transpose()?
                .unwrap_or_default();
            let blobs = blobs
                .as_ref()
                .and_then(|table| table.get(serial).transpose())
                .transpose()?
                .map(|value| serde_json::from_slice(value.value()))
                .transpose()?
                .unwrap_or_default();
            state.apply(mutations, blobs, payload.value())?;
        }
        Ok(())
    }

    /// Folds the journal from empty through `through`, ignoring any published checkpoint.
    ///
    /// # Errors
    /// Returns a store error if the read fails, or a decode error for a malformed record.
    pub fn folded_state(&self, through: u64) -> Result<CheckpointState, MetaError> {
        let mut state = CheckpointState::default();
        self.fold_journal(&mut state, 0, through)?;
        Ok(state)
    }

    /// Makes the state at the current serial durable, folding only the records the published
    /// checkpoint does not already cover.
    ///
    /// One transaction writes the state and the manifest that names it, so a crash leaves either the
    /// previous checkpoint or this one. No journal row is removed and no floor moves: a replica that
    /// has fallen behind still has the whole history to read.
    ///
    /// # Errors
    /// Returns a store error if the read, write or commit fails, or a decode error for a malformed
    /// record.
    pub fn publish_checkpoint(&self, identity: CheckpointIdentity) -> Result<CheckpointManifest, MetaError> {
        let published = self.checkpoint()?;
        let base = published.as_ref().map_or(0, |checkpoint| checkpoint.manifest.serial);
        let mut state = published.map_or_else(CheckpointState::default, |checkpoint| checkpoint.state);
        let serial = self.current_serial()?;
        self.fold_journal(&mut state, base, serial)?;
        let manifest = state.manifest(identity, serial);
        let txn = self.db.begin_write()?;
        txn.delete_table(CHECKPOINT_ROW)?;
        txn.delete_table(CHECKPOINT_REVOCATION)?;
        txn.delete_table(CHECKPOINT_BLOB)?;
        {
            let mut rows = txn.open_table(CHECKPOINT_ROW)?;
            for (key, value) in &state.rows {
                rows.insert(key.as_str(), value.as_slice())?;
            }
            let mut revocations = txn.open_table(CHECKPOINT_REVOCATION)?;
            for (digest, record) in &state.revocations {
                revocations.insert(digest.as_str(), serde_json::to_vec(record)?.as_slice())?;
            }
            let mut blobs = txn.open_table(CHECKPOINT_BLOB)?;
            for blob in &state.blobs {
                blobs.insert(blob.sha256.as_str(), blob.size)?;
            }
            txn.open_table(CHECKPOINT_META)?
                .insert(MANIFEST_KEY, serde_json::to_vec(&manifest)?.as_slice())?;
        }
        txn.commit()?;
        Ok(manifest)
    }

    /// Returns the published checkpoint, or `None` before the first publication.
    ///
    /// # Errors
    /// Returns a store error if the read fails, or a decode error for a malformed row.
    pub fn checkpoint(&self) -> Result<Option<Checkpoint>, MetaError> {
        let Some(manifest) = self.checkpoint_manifest()? else {
            return Ok(None);
        };
        // One transaction publishes the manifest and the three state tables, so a manifest without
        // them is a corrupt store rather than an empty checkpoint, and opening them plainly says so.
        let txn = self.db.begin_read()?;
        let mut rows = BTreeMap::new();
        for entry in txn.open_table(CHECKPOINT_ROW)?.iter()? {
            let (key, value) = entry?;
            rows.insert(key.value().to_owned(), value.value().to_vec());
        }
        let mut revocations = BTreeMap::new();
        for entry in txn.open_table(CHECKPOINT_REVOCATION)?.iter()? {
            let (digest, record) = entry?;
            revocations.insert(digest.value().to_owned(), serde_json::from_slice(record.value())?);
        }
        let mut blobs = BTreeSet::new();
        for entry in txn.open_table(CHECKPOINT_BLOB)?.iter()? {
            let (sha256, size) = entry?;
            blobs.insert(DriverBlobReference {
                sha256: sha256.value().to_owned(),
                size: size.value(),
            });
        }
        Ok(Some(Checkpoint {
            manifest,
            state: CheckpointState::from_parts(rows, revocations, blobs),
        }))
    }

    /// Returns the published manifest without reading the state beside it.
    ///
    /// # Errors
    /// Returns a store error if the read fails, or a decode error for a malformed manifest.
    pub fn checkpoint_manifest(&self) -> Result<Option<CheckpointManifest>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, CHECKPOINT_META)? else {
            return Ok(None);
        };
        table
            .get(MANIFEST_KEY)?
            .map(|value| serde_json::from_slice(value.value()).map_err(MetaError::from))
            .transpose()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/checkpoint/tests.rs"]
mod tests;
