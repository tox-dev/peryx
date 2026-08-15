use std::num::NonZeroUsize;
use std::ops::Bound::{Excluded, Included, Unbounded};

use peryx_ha::{
    BlobPlacementGroupPage, BlobPlacementKey, BlobPlacementPage, BlobPlacementRecord, BlobPlacementStore, CompareWrite,
    MAX_PLACEMENTS_PER_DIGEST,
};
use peryx_identity::ArtifactDigest;
use redb::ReadableTable as _;

use super::{BLOB_PLACEMENT, MetaError, MetaStore};

impl MetaStore {
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn blob_placement(&self, key: &BlobPlacementKey) -> Result<Option<BlobPlacementRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(BLOB_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(table
            .get(encoded_key(key).as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn blob_placements(&self, digest: &ArtifactDigest) -> Result<Vec<BlobPlacementRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(BLOB_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let (low, high) = digest_bounds(digest);
        let mut records = Vec::new();
        for entry in table.range::<&str>((Included(low.as_str()), Excluded(high.as_str())))? {
            let (_, value) = entry?;
            records.push(serde_json::from_slice(value.value())?);
        }
        Ok(records)
    }

    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn scan_blob_placements(
        &self,
        cursor: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<BlobPlacementPage, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(BLOB_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BlobPlacementPage::default()),
            Err(error) => return Err(error.into()),
        };
        let entries = match cursor {
            Some(after) => table.range::<&str>((Excluded(after), Unbounded))?,
            None => table.iter()?,
        };
        let mut page = BlobPlacementPage::default();
        for entry in entries {
            let (key, value) = entry?;
            page.records.push(serde_json::from_slice(value.value())?);
            if page.records.len() == limit.get() {
                page.next_cursor = Some(key.value().to_owned());
                break;
            }
        }
        Ok(page)
    }

    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn scan_blob_placement_groups(
        &self,
        cursor: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<BlobPlacementGroupPage, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(BLOB_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BlobPlacementGroupPage::default()),
            Err(error) => return Err(error.into()),
        };
        let resume = cursor.map(|digest| format!("{digest}\u{1}"));
        let entries = match resume.as_deref() {
            Some(after) => table.range::<&str>((Included(after), Unbounded))?,
            None => table.iter()?,
        };
        let mut page = BlobPlacementGroupPage::default();
        let mut group: Vec<BlobPlacementRecord> = Vec::new();
        for entry in entries {
            let (_, value) = entry?;
            let record: BlobPlacementRecord = serde_json::from_slice(value.value())?;
            if let Some(first) = group.first()
                && first.key.digest != record.key.digest
            {
                let cursor = first.key.digest.canonical();
                page.groups.push(std::mem::take(&mut group));
                if page.groups.len() == limit.get() {
                    page.next_cursor = Some(cursor);
                    return Ok(page);
                }
            }
            group.push(record);
        }
        if !group.is_empty() {
            page.groups.push(group);
        }
        Ok(page)
    }

    /// Commits a caller-decided state only when the persisted record still matches `expected`.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot be read, encoded, or committed.
    pub fn compare_and_put_blob_placement(
        &self,
        expected: Option<&BlobPlacementRecord>,
        replacement: &BlobPlacementRecord,
    ) -> Result<CompareWrite, MetaError> {
        let txn = self.db.begin_write()?;
        let outcome = {
            let mut table = txn.open_table(BLOB_PLACEMENT)?;
            let key = encoded_key(&replacement.key);
            let current = table
                .get(key.as_str())?
                .map(|value| serde_json::from_slice::<BlobPlacementRecord>(value.value()))
                .transpose()?;
            if current.as_ref() != expected {
                CompareWrite::Conflict
            } else if expected.is_none()
                && placement_count(&table, &replacement.key.digest)? >= MAX_PLACEMENTS_PER_DIGEST
            {
                CompareWrite::CapacityExceeded
            } else {
                table.insert(key.as_str(), serde_json::to_vec(replacement)?.as_slice())?;
                CompareWrite::Written
            }
        };
        txn.commit()?;
        Ok(outcome)
    }
}

impl BlobPlacementStore for MetaStore {
    type Error = MetaError;

    fn blob_placement(&self, key: &BlobPlacementKey) -> Result<Option<BlobPlacementRecord>, Self::Error> {
        Self::blob_placement(self, key)
    }

    fn blob_placements(&self, digest: &ArtifactDigest) -> Result<Vec<BlobPlacementRecord>, Self::Error> {
        Self::blob_placements(self, digest)
    }

    fn scan_blob_placements(
        &self,
        cursor: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<BlobPlacementPage, Self::Error> {
        Self::scan_blob_placements(self, cursor, limit)
    }

    fn scan_blob_placement_groups(
        &self,
        cursor: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<BlobPlacementGroupPage, Self::Error> {
        Self::scan_blob_placement_groups(self, cursor, limit)
    }

    fn compare_and_put_blob_placement(
        &self,
        expected: Option<&BlobPlacementRecord>,
        replacement: &BlobPlacementRecord,
    ) -> Result<CompareWrite, Self::Error> {
        Self::compare_and_put_blob_placement(self, expected, replacement)
    }
}

fn encoded_key(key: &BlobPlacementKey) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        key.digest.canonical(),
        key.backend.as_str(),
        key.data_center.as_str(),
        key.location.as_str()
    )
}

fn digest_bounds(digest: &ArtifactDigest) -> (String, String) {
    let canonical = digest.canonical();
    (format!("{canonical}\0"), format!("{canonical}\u{1}"))
}

fn placement_count(
    table: &redb::Table<'_, &'static str, &'static [u8]>,
    digest: &ArtifactDigest,
) -> Result<usize, MetaError> {
    let (low, high) = digest_bounds(digest);
    Ok(table
        .range::<&str>((Included(low.as_str()), Excluded(high.as_str())))?
        .count())
}
