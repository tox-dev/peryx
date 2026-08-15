use peryx_ha::{ReclaimGuard, ReclaimGuardArm, ReclaimGuardStore};
use redb::ReadableTable as _;

use super::{BLOB_RECLAIM_GUARD, MetaError, MetaStore, SERIAL, SERIAL_KEY, open_optional_table};

impl ReclaimGuardStore for MetaStore {
    type Error = MetaError;

    fn reclaim_guard_serial(&self) -> Result<u64, Self::Error> {
        self.current_serial()
    }

    fn compare_and_arm_reclaim_guards(
        &self,
        digests: &[&str],
        expected_serial: u64,
        now: i64,
        replacement: ReclaimGuard,
    ) -> Result<ReclaimGuardArm, Self::Error> {
        let txn = self.db.begin_write()?;
        let serial = {
            let table = txn.open_table(SERIAL)?;
            let value = table.get(SERIAL_KEY)?;
            value.map_or(0, |value| value.value())
        };
        if serial != expected_serial {
            txn.commit()?;
            return Ok(ReclaimGuardArm::SerialChanged);
        }
        let mut armed = Vec::new();
        if !digests.is_empty() {
            let mut table = txn.open_table(BLOB_RECLAIM_GUARD)?;
            for &digest in digests {
                let available = {
                    let value = table.get(digest)?;
                    value.is_none_or(|value| {
                        ReclaimGuard {
                            expires_at_unix: value.value(),
                        }
                        .is_expired_at(now)
                    })
                };
                if available {
                    table.insert(digest, replacement.expires_at_unix)?;
                    armed.push(digest.to_owned());
                }
            }
        }
        txn.commit()?;
        Ok(ReclaimGuardArm::Armed(armed))
    }

    fn compare_and_disarm_reclaim_guard(&self, digest: &str, expected: ReclaimGuard) -> Result<bool, Self::Error> {
        let exists = {
            let txn = self.db.begin_read()?;
            let table = open_optional_table(&txn, BLOB_RECLAIM_GUARD)?;
            table.is_some()
        };
        if !exists {
            return Ok(false);
        }
        let txn = self.db.begin_write()?;
        let removed = {
            let mut table = txn.open_table(BLOB_RECLAIM_GUARD)?;
            let matches = {
                let value = table.get(digest)?;
                value.is_some_and(|value| value.value() == expected.expires_at_unix)
            };
            if matches {
                table.remove(digest)?;
                true
            } else {
                false
            }
        };
        txn.commit()?;
        Ok(removed)
    }

    fn reclaim_guard(&self, digest: &str) -> Result<Option<ReclaimGuard>, Self::Error> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, BLOB_RECLAIM_GUARD)? else {
            return Ok(None);
        };
        let value = table.get(digest)?.map(|value| ReclaimGuard {
            expires_at_unix: value.value(),
        });
        Ok(value)
    }

    fn reclaim_guards(&self) -> Result<Vec<(String, ReclaimGuard)>, Self::Error> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, BLOB_RECLAIM_GUARD)? else {
            return Ok(Vec::new());
        };
        table
            .iter()?
            .map(|entry| {
                let (digest, expires_at) = entry?;
                Ok((
                    digest.value().to_owned(),
                    ReclaimGuard {
                        expires_at_unix: expires_at.value(),
                    },
                ))
            })
            .collect()
    }
}
