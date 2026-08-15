//! Writer claims do not expire. Failover atomically replaces a known identity so a delayed former writer
//! cannot reclaim the store without operator promotion.

use redb::ReadableTable as _;

use super::{MetaError, MetaStore, WRITER, WRITER_KEY, WriterIdentityError};

impl MetaStore {
    /// Returns `None` until an identity claims the store.
    ///
    /// # Errors
    /// Returns a store error if the identity cannot be read.
    pub fn writer_identity(&self) -> Result<Option<String>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(WRITER) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(table.get(WRITER_KEY)?.map(|identity| identity.value().to_owned()))
    }

    /// Repeated claims by the current identity succeed.
    ///
    /// # Errors
    /// Returns [`WriterIdentityError::Empty`] for an empty identity, a conflict when another writer
    /// owns the store, or a store error if the transaction fails.
    pub fn claim_writer_identity(&self, identity: &str) -> Result<(), WriterIdentityError> {
        validate(identity)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        {
            let mut table = txn.open_table(WRITER).map_err(MetaError::from)?;
            let active = table
                .get(WRITER_KEY)
                .map_err(MetaError::from)?
                .map(|value| value.value().to_owned());
            match active {
                None => {
                    table.insert(WRITER_KEY, identity).map_err(MetaError::from)?;
                }
                Some(active) if active == identity => {}
                Some(active) => {
                    return Err(WriterIdentityError::Claimed {
                        active,
                        requested: identity.to_owned(),
                    });
                }
            }
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(())
    }

    /// Replaces `expected` atomically during manual failover.
    ///
    /// # Errors
    /// Returns [`WriterIdentityError::Empty`] for an empty identity, a stale-identity error if the
    /// current writer differs from `expected`, or a store error if the transaction fails.
    pub fn promote_writer_identity(&self, expected: &str, replacement: &str) -> Result<(), WriterIdentityError> {
        validate(expected)?;
        validate(replacement)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        {
            let mut table = txn.open_table(WRITER).map_err(MetaError::from)?;
            let active = table
                .get(WRITER_KEY)
                .map_err(MetaError::from)?
                .map(|value| value.value().to_owned());
            if active.as_deref() != Some(expected) {
                return Err(WriterIdentityError::Changed {
                    active,
                    expected: expected.to_owned(),
                });
            }
            table.insert(WRITER_KEY, replacement).map_err(MetaError::from)?;
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(())
    }
}

const fn validate(identity: &str) -> Result<(), WriterIdentityError> {
    if identity.is_empty() {
        return Err(WriterIdentityError::Empty);
    }
    Ok(())
}
