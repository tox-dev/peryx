//! Row-level analytics behaviour that needs the crate-private table definitions: what a metadata
//! migration leaves behind, and what one checkpoint actually writes.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::backends::InMemoryBackend;

use crate::meta::{
    ANALYTICS, ANALYTICS_DAILY_KEY, ANALYTICS_KEY, AnalyticsCheckpoint, AnalyticsDelta, ArtifactUsageKey,
    DailyUsageKey, MetaStore, UsageTotals,
};

/// Counts what redb hands the disk, so a checkpoint's cost is measured rather than asserted.
#[derive(Debug)]
struct CountingBackend {
    inner: InMemoryBackend,
    written: Arc<AtomicU64>,
}

impl redb::StorageBackend for CountingBackend {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.inner.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.written.fetch_add(data.len() as u64, Ordering::SeqCst);
        self.inner.write(offset, data)
    }
}

fn counted() -> (MetaStore, Arc<AtomicU64>) {
    let written = Arc::new(AtomicU64::new(0));
    let backend = CountingBackend {
        inner: InMemoryBackend::new(),
        written: Arc::clone(&written),
    };
    let database = redb::Database::builder().create_with_backend(backend).unwrap();
    (MetaStore::initialize(database).unwrap(), written)
}

fn write_migrated(store: &MetaStore, lifetime: &[u8], daily: &[u8]) {
    let txn = store.db.begin_write().unwrap();
    {
        let mut table = txn.open_table(ANALYTICS).unwrap();
        table.insert(ANALYTICS_KEY, lifetime).unwrap();
        table.insert(ANALYTICS_DAILY_KEY, daily).unwrap();
    }
    txn.commit().unwrap();
}

fn history(rows: usize) -> AnalyticsDelta {
    AnalyticsDelta {
        lifetime: (0..rows)
            .map(|index| {
                (
                    ArtifactUsageKey {
                        repository: "alpha".to_owned(),
                        resource: format!("resource-{index}"),
                        artifact: format!("resource-{index}-1.0.bin"),
                    },
                    UsageTotals { reads: 1, bytes: 4096 },
                )
            })
            .collect(),
        daily: (0..rows)
            .map(|index| {
                (
                    DailyUsageKey {
                        day: 19_000,
                        repository: "alpha".to_owned(),
                        resource: format!("resource-{index}"),
                        group: "1.0".to_owned(),
                        source: "upstream".to_owned(),
                    },
                    UsageTotals { reads: 1, bytes: 4096 },
                )
            })
            .collect(),
        ..AnalyticsDelta::default()
    }
}

fn one_row_checkpoint_bytes(rows: usize) -> u64 {
    let (store, written) = counted();
    store.analytics().commit_checkpoint(&history(rows)).unwrap();
    let mut changed = history(1);
    changed.lifetime[0].1 = UsageTotals { reads: 2, bytes: 8192 };
    changed.daily[0].1 = UsageTotals { reads: 2, bytes: 8192 };
    written.store(0, Ordering::SeqCst);
    store.analytics().commit_checkpoint(&changed).unwrap();
    written.load(Ordering::SeqCst)
}

/// Quadrupling the stored history must leave the cost of one changed row where it was. A full-value
/// checkpoint quadruples it instead.
#[test]
fn test_one_changed_row_costs_the_same_bytes_however_much_history_is_stored() {
    let (small, large) = (one_row_checkpoint_bytes(4_000), one_row_checkpoint_bytes(16_000));

    assert_eq!((large, large > 0), (small, true));
}

#[test]
fn test_load_reports_values_a_metadata_migration_left_under_the_pre_row_keys() {
    let (store, _written) = counted();
    write_migrated(&store, b"migrated lifetime", b"migrated daily");

    assert_eq!(
        store.analytics().load_checkpoint().unwrap(),
        AnalyticsCheckpoint {
            lifetime: Vec::new(),
            daily: Vec::new(),
            migrated_lifetime: Some(b"migrated lifetime".to_vec()),
            migrated_daily: Some(b"migrated daily".to_vec()),
        }
    );
}

#[test]
fn test_commit_clears_migrated_values_in_the_transaction_that_writes_their_rows() {
    let (store, _written) = counted();
    write_migrated(&store, b"migrated lifetime", b"migrated daily");
    let adopted = history(2);
    store
        .analytics()
        .commit_checkpoint(&AnalyticsDelta {
            clear_migrated: true,
            ..adopted.clone()
        })
        .unwrap();

    assert_eq!(
        store.analytics().load_checkpoint().unwrap(),
        AnalyticsCheckpoint {
            lifetime: adopted.lifetime,
            daily: adopted.daily,
            migrated_lifetime: None,
            migrated_daily: None,
        }
    );
}
