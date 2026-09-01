use std::sync::Weak;

use redb::ReadableTable as _;

use super::error::MetaError;
use super::{
    ANALYTICS, ANALYTICS_APPLY_KEY, ANALYTICS_DAILY, ANALYTICS_DAILY_KEY, ANALYTICS_KEY, ANALYTICS_LIFETIME,
    ANALYTICS_PRODUCER_KEY, MetaDatabase, MetaStore,
};

/// Identifies one all-time per-artifact counter.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactUsageKey {
    pub repository: String,
    pub resource: String,
    pub artifact: String,
}

/// Identifies one daily usage bucket. `day` leads the key so retention drops an expired prefix as a
/// single range and never walks a retained bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DailyUsageKey {
    pub day: i64,
    pub repository: String,
    pub resource: String,
    pub group: String,
    pub source: String,
}

/// Reads and served bytes accumulated against one key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub reads: u64,
    pub bytes: u64,
}

/// Every durable analytics row, read once at startup.
///
/// `migrated_lifetime` and `migrated_daily` carry whatever a metadata migration left under the
/// pre-row keys, which the owner folds into rows and clears through [`AnalyticsDelta`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalyticsCheckpoint {
    pub lifetime: Vec<(ArtifactUsageKey, UsageTotals)>,
    pub daily: Vec<(DailyUsageKey, UsageTotals)>,
    pub migrated_lifetime: Option<Vec<u8>>,
    pub migrated_daily: Option<Vec<u8>>,
}

/// The rows one checkpoint changes, so a commit costs what the interval touched rather than what
/// the store holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalyticsDelta {
    pub lifetime: Vec<(ArtifactUsageKey, UsageTotals)>,
    pub daily: Vec<(DailyUsageKey, UsageTotals)>,
    /// Removes every daily bucket before this UTC day.
    pub expire_daily_before: Option<i64>,
    /// Removes the pre-row values once their rows are committed.
    pub clear_migrated: bool,
}

impl AnalyticsDelta {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.lifetime.is_empty() && self.daily.is_empty() && self.expire_daily_before.is_none() && !self.clear_migrated
    }
}

/// Does not keep the database open.
#[derive(Debug, Clone)]
pub struct AnalyticsHandle {
    db: Weak<MetaDatabase>,
}

impl MetaStore {
    #[must_use]
    pub fn analytics(&self) -> AnalyticsHandle {
        AnalyticsHandle {
            db: std::sync::Arc::downgrade(&self.db),
        }
    }
}

impl AnalyticsHandle {
    /// Loads every analytics row from one storage snapshot. Returns an empty checkpoint before the
    /// first commit or after the store drops.
    ///
    /// # Errors
    /// Returns a store error if any row cannot be read.
    pub fn load_checkpoint(&self) -> Result<AnalyticsCheckpoint, MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(AnalyticsCheckpoint::default());
        };
        let txn = db.begin_read()?;
        let mut checkpoint = AnalyticsCheckpoint::default();
        for entry in txn.open_table(ANALYTICS_LIFETIME)?.iter()? {
            let (key, totals) = entry?;
            let (repository, resource, artifact) = key.value();
            checkpoint.lifetime.push((
                ArtifactUsageKey {
                    repository: repository.to_owned(),
                    resource: resource.to_owned(),
                    artifact: artifact.to_owned(),
                },
                totals_of(totals.value()),
            ));
        }
        for entry in txn.open_table(ANALYTICS_DAILY)?.iter()? {
            let (key, totals) = entry?;
            let (day, repository, resource, group, source) = key.value();
            checkpoint.daily.push((
                DailyUsageKey {
                    day,
                    repository: repository.to_owned(),
                    resource: resource.to_owned(),
                    group: group.to_owned(),
                    source: source.to_owned(),
                },
                totals_of(totals.value()),
            ));
        }
        let migrated = txn.open_table(ANALYTICS)?;
        checkpoint.migrated_lifetime = migrated.get(ANALYTICS_KEY)?.map(|value| value.value().to_vec());
        checkpoint.migrated_daily = migrated.get(ANALYTICS_DAILY_KEY)?.map(|value| value.value().to_vec());
        Ok(checkpoint)
    }

    /// Commits the changed rows, the retention prune, and any migrated-value removal as one
    /// checkpoint at a single event frontier. Does nothing after the store drops.
    ///
    /// # Errors
    /// Returns a store error if any row cannot be written or the transaction cannot commit.
    pub fn commit_checkpoint(&self, delta: &AnalyticsDelta) -> Result<(), MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(());
        };
        let txn = db.begin_write()?;
        {
            let mut lifetime = txn.open_table(ANALYTICS_LIFETIME)?;
            for (key, totals) in &delta.lifetime {
                lifetime.insert(
                    (key.repository.as_str(), key.resource.as_str(), key.artifact.as_str()),
                    (totals.reads, totals.bytes),
                )?;
            }
            let mut daily = txn.open_table(ANALYTICS_DAILY)?;
            if let Some(floor) = delta.expire_daily_before {
                daily.retain_in(..(floor, "", "", "", ""), |_, _| false)?;
            }
            for (key, totals) in &delta.daily {
                daily.insert(
                    (
                        key.day,
                        key.repository.as_str(),
                        key.resource.as_str(),
                        key.group.as_str(),
                        key.source.as_str(),
                    ),
                    (totals.reads, totals.bytes),
                )?;
            }
            if delta.clear_migrated {
                let mut migrated = txn.open_table(ANALYTICS)?;
                migrated.remove(ANALYTICS_KEY)?;
                migrated.remove(ANALYTICS_DAILY_KEY)?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Returns `None` before the first save or after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn load_apply(&self) -> Result<Option<Vec<u8>>, MetaError> {
        self.read(ANALYTICS_APPLY_KEY)
    }

    /// Does nothing after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save_apply(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        self.write(ANALYTICS_APPLY_KEY, snapshot)
    }

    /// Returns `None` before the first save or after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn load_producer(&self) -> Result<Option<Vec<u8>>, MetaError> {
        self.read(ANALYTICS_PRODUCER_KEY)
    }

    /// Does nothing after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save_producer(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        self.write(ANALYTICS_PRODUCER_KEY, snapshot)
    }

    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(None);
        };
        let txn = db.begin_read()?;
        let table = txn.open_table(ANALYTICS)?;
        Ok(table.get(key)?.map(|value| value.value().to_vec()))
    }

    fn write(&self, key: &str, snapshot: &[u8]) -> Result<(), MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(());
        };
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(ANALYTICS)?;
            table.insert(key, snapshot)?;
        }
        txn.commit()?;
        Ok(())
    }
}

const fn totals_of((reads, bytes): (u64, u64)) -> UsageTotals {
    UsageTotals { reads, bytes }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/analytics_row_tests.rs"]
mod row_tests;

#[cfg(test)]
#[path = "../../tests/unit/meta/analytics_fault_tests.rs"]
mod fault_tests;
