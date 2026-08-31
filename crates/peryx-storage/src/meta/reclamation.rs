use std::num::NonZeroUsize;
use std::ops::Bound::{Excluded, Included, Unbounded};

use peryx_ha::{
    BlobPlacementRecord, ReclamationSnapshot, ReclamationStore, ReclamationTombstone, ReclamationTombstonePage,
    TombstoneWrite,
};
use peryx_identity::ArtifactDigest;
use redb::ReadableTable as _;

use super::index::write_reference_revision;
use super::{BLOB_PLACEMENT, MetaError, MetaStore, RECLAMATION_TOMBSTONE, open_optional_table};

impl ReclamationStore for MetaStore {
    type Error = MetaError;

    fn reclamation_snapshot(&self, digest: &ArtifactDigest) -> Result<ReclamationSnapshot, Self::Error> {
        let txn = self.db.begin_read()?;
        let tombstone = match open_optional_table(&txn, RECLAMATION_TOMBSTONE)? {
            Some(table) => {
                let value = table.get(digest.canonical().as_str())?;
                value.map(|value| serde_json::from_slice(value.value())).transpose()?
            }
            None => None,
        };
        let placements = match open_optional_table(&txn, BLOB_PLACEMENT)? {
            Some(table) => read_placements(&table, digest)?,
            None => Vec::new(),
        };
        Ok(ReclamationSnapshot { tombstone, placements })
    }

    fn compare_and_put_reclamation_tombstone(
        &self,
        expected: &ReclamationSnapshot,
        replacement: &ReclamationTombstone,
        revision: u64,
    ) -> Result<TombstoneWrite, Self::Error> {
        let txn = self.db.begin_write()?;
        let outcome = if write_reference_revision(&txn)? != revision {
            TombstoneWrite::ReferencesMoved
        } else if write_snapshot_matches(&txn, expected, &replacement.digest)? {
            {
                let mut table = txn.open_table(RECLAMATION_TOMBSTONE)?;
                let key = replacement.digest.canonical();
                let encoded = serde_json::to_vec(replacement)?;
                table.insert(key.as_str(), encoded.as_slice())?;
            }
            TombstoneWrite::Written
        } else {
            TombstoneWrite::Conflict
        };
        txn.commit()?;
        Ok(outcome)
    }

    fn compare_and_remove_reclamation_tombstone(&self, expected: &ReclamationTombstone) -> Result<bool, Self::Error> {
        let exists = {
            let txn = self.db.begin_read()?;
            let table = open_optional_table(&txn, RECLAMATION_TOMBSTONE)?;
            table.is_some()
        };
        if !exists {
            return Ok(false);
        }
        let txn = self.db.begin_write()?;
        let removed = {
            let mut table = txn.open_table(RECLAMATION_TOMBSTONE)?;
            let current = {
                let value = table.get(expected.digest.canonical().as_str())?;
                value
                    .map(|value| serde_json::from_slice::<ReclamationTombstone>(value.value()))
                    .transpose()?
            };
            if current.as_ref() == Some(expected) {
                table.remove(expected.digest.canonical().as_str())?;
                true
            } else {
                false
            }
        };
        txn.commit()?;
        Ok(removed)
    }

    fn reclamation_tombstone(&self, digest: &ArtifactDigest) -> Result<Option<ReclamationTombstone>, Self::Error> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECLAMATION_TOMBSTONE)? else {
            return Ok(None);
        };
        let tombstone = table
            .get(digest.canonical().as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?;
        Ok(tombstone)
    }

    fn reclamation_tombstones(&self) -> Result<Vec<ReclamationTombstone>, Self::Error> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECLAMATION_TOMBSTONE)? else {
            return Ok(Vec::new());
        };
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            records.push(serde_json::from_slice(value.value())?);
        }
        Ok(records)
    }

    fn scan_reclamation_tombstones(
        &self,
        cursor: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<ReclamationTombstonePage, Self::Error> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECLAMATION_TOMBSTONE)? else {
            return Ok(ReclamationTombstonePage::default());
        };
        let entries = match cursor {
            Some(after) => table.range::<&str>((Excluded(after), Unbounded))?,
            None => table.iter()?,
        };
        let mut page = ReclamationTombstonePage::default();
        let mut last_key = None;
        for entry in entries {
            let (key, value) = entry?;
            if page.records.len() == limit.get() {
                page.next_cursor = last_key;
                break;
            }
            page.records.push(serde_json::from_slice(value.value())?);
            last_key = Some(key.value().to_owned());
        }
        Ok(page)
    }
}

fn write_snapshot_matches(
    txn: &redb::WriteTransaction,
    expected: &ReclamationSnapshot,
    digest: &ArtifactDigest,
) -> Result<bool, MetaError> {
    let tombstone = {
        let table = txn.open_table(RECLAMATION_TOMBSTONE)?;
        let value = table.get(digest.canonical().as_str())?;
        value
            .map(|value| serde_json::from_slice::<ReclamationTombstone>(value.value()))
            .transpose()?
    };
    if tombstone != expected.tombstone {
        return Ok(false);
    }
    let placements = {
        let table = txn.open_table(BLOB_PLACEMENT)?;
        read_placements(&table, digest)?
    };
    Ok(placements == expected.placements)
}

fn read_placements<T>(table: &T, digest: &ArtifactDigest) -> Result<Vec<BlobPlacementRecord>, MetaError>
where
    T: redb::ReadableTable<&'static str, &'static [u8]>,
{
    let canonical = digest.canonical();
    let low = format!("{canonical}\0");
    let high = format!("{canonical}\u{1}");
    let mut placements = Vec::new();
    for entry in table.range::<&str>((Included(low.as_str()), Excluded(high.as_str())))? {
        let (_, value) = entry?;
        placements.push(serde_json::from_slice(value.value())?);
    }
    Ok(placements)
}
