use std::ops::Bound::{Excluded, Unbounded};

use peryx_ha::{
    ArtifactPlacement, ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementQuery, ArtifactPlacementRow,
    ArtifactPlacementStore, ByteAvailability,
};
use redb::{ReadableTable as _, ReadableTableMetadata as _};

use super::{ARTIFACT_PLACEMENT, MetaError, MetaStore};

const MAX_QUERY_LIMIT: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum ArtifactPlacementQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

impl MetaStore {
    /// # Errors
    /// Returns a store error when the write fails.
    pub fn put_artifact_placement(&self, digest: &str, placement: &ArtifactPlacement) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(ARTIFACT_PLACEMENT)?;
            table.insert(digest, serde_json::to_vec(placement)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Writes the placements a driver transaction staged into that same transaction. An empty list opens
    /// nothing, so a driver mutation that stages no placement leaves the optional table uncreated.
    pub(super) fn write_artifact_placements(
        txn: &redb::WriteTransaction,
        placements: &[(String, ArtifactPlacement)],
    ) -> Result<(), MetaError> {
        if placements.is_empty() {
            return Ok(());
        }
        let mut table = txn.open_table(ARTIFACT_PLACEMENT)?;
        for (digest, placement) in placements {
            table.insert(digest.as_str(), serde_json::to_vec(placement)?.as_slice())?;
        }
        Ok(())
    }

    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn get_artifact_placement(&self, digest: &str) -> Result<Option<ArtifactPlacement>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(ARTIFACT_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let placement = table
            .get(digest)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?;
        Ok(placement)
    }

    /// Inserts `placement` only when the digest has no row.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot be read, encoded, or committed.
    pub fn insert_artifact_placement(
        &self,
        digest: &str,
        placement: &ArtifactPlacement,
    ) -> Result<ArtifactPlacement, MetaError> {
        let txn = self.db.begin_write()?;
        let stored = {
            let mut table = txn.open_table(ARTIFACT_PLACEMENT)?;
            let current = table
                .get(digest)?
                .map(|value| serde_json::from_slice::<ArtifactPlacement>(value.value()))
                .transpose()?;
            if let Some(existing) = current {
                existing
            } else {
                table.insert(digest, serde_json::to_vec(placement)?.as_slice())?;
                *placement
            }
        };
        txn.commit()?;
        Ok(stored)
    }

    /// Replaces a row only when it still equals `expected`.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot be read, encoded, or committed.
    pub fn compare_and_put_artifact_placement(
        &self,
        digest: &str,
        expected: &ArtifactPlacement,
        replacement: &ArtifactPlacement,
    ) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let written = {
            let mut table = txn.open_table(ARTIFACT_PLACEMENT)?;
            let current = {
                let value = table.get(digest)?;
                value
                    .map(|value| serde_json::from_slice::<ArtifactPlacement>(value.value()))
                    .transpose()?
            };
            if current.as_ref() == Some(expected) {
                table.insert(digest, serde_json::to_vec(replacement)?.as_slice())?;
                true
            } else {
                false
            }
        };
        txn.commit()?;
        Ok(written)
    }

    /// # Errors
    /// Returns a store error when the write fails.
    pub fn delete_artifact_placement(&self, digest: &str) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let removed = {
            let mut table = txn.open_table(ARTIFACT_PLACEMENT)?;
            table.remove(digest)?.is_some()
        };
        txn.commit()?;
        Ok(removed)
    }

    /// # Errors
    /// Returns a store error when the table cannot be read.
    pub fn count_artifact_placements(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(ARTIFACT_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let count = table.len()?;
        Ok(count)
    }

    /// # Errors
    /// Returns an invalid-limit error or a store error when a row cannot be read or decoded.
    pub fn list_artifact_placements(
        &self,
        query: &ArtifactPlacementQuery,
    ) -> Result<ArtifactPlacementPage, ArtifactPlacementQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(ArtifactPlacementQueryError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = match txn.open_table(ARTIFACT_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(ArtifactPlacementPage {
                    rows: Vec::new(),
                    next_cursor: None,
                });
            }
            Err(error) => return Err(MetaError::from(error).into()),
        };
        let entries = query
            .cursor
            .as_ref()
            .map_or_else(
                || table.iter(),
                |cursor| table.range::<&str>((Excluded(cursor.as_str()), Unbounded)),
            )
            .map_err(MetaError::from)?;
        let mut rows = Vec::with_capacity(query.limit + 1);
        for entry in entries {
            let (key, value) = entry.map_err(MetaError::from)?;
            let placement: ArtifactPlacement = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            rows.push(ArtifactPlacementRow {
                digest: key.value().to_owned(),
                source: placement.source,
                availability: placement.availability,
            });
            if rows.len() > query.limit {
                break;
            }
        }
        let next_cursor = (rows.len() > query.limit).then(|| rows[query.limit - 1].digest.clone());
        rows.truncate(query.limit);
        Ok(ArtifactPlacementPage { rows, next_cursor })
    }

    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn artifact_placement_health(&self) -> Result<ArtifactPlacementHealth, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(ARTIFACT_PLACEMENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(ArtifactPlacementHealth::default()),
            Err(error) => return Err(error.into()),
        };
        let mut health = ArtifactPlacementHealth::default();
        for entry in table.iter()? {
            let (_, value) = entry?;
            let placement: ArtifactPlacement = serde_json::from_slice(value.value())?;
            match placement.availability {
                ByteAvailability::Local => health.local += 1,
                ByteAvailability::RemoteOnly => health.remote_only += 1,
                ByteAvailability::Unavailable => health.unavailable += 1,
            }
        }
        Ok(health)
    }
}

impl ArtifactPlacementStore for MetaStore {
    type Error = MetaError;
    type QueryError = ArtifactPlacementQueryError;

    fn put_artifact_placement(&self, digest: &str, placement: &ArtifactPlacement) -> Result<(), Self::Error> {
        Self::put_artifact_placement(self, digest, placement)
    }

    fn get_artifact_placement(&self, digest: &str) -> Result<Option<ArtifactPlacement>, Self::Error> {
        Self::get_artifact_placement(self, digest)
    }

    fn insert_artifact_placement(
        &self,
        digest: &str,
        placement: &ArtifactPlacement,
    ) -> Result<ArtifactPlacement, Self::Error> {
        Self::insert_artifact_placement(self, digest, placement)
    }

    fn compare_and_put_artifact_placement(
        &self,
        digest: &str,
        expected: &ArtifactPlacement,
        replacement: &ArtifactPlacement,
    ) -> Result<bool, Self::Error> {
        Self::compare_and_put_artifact_placement(self, digest, expected, replacement)
    }

    fn delete_artifact_placement(&self, digest: &str) -> Result<bool, Self::Error> {
        Self::delete_artifact_placement(self, digest)
    }

    fn list_artifact_placements(
        &self,
        query: &ArtifactPlacementQuery,
    ) -> Result<ArtifactPlacementPage, Self::QueryError> {
        Self::list_artifact_placements(self, query)
    }

    fn artifact_placement_health(&self) -> Result<ArtifactPlacementHealth, Self::Error> {
        Self::artifact_placement_health(self)
    }
}
