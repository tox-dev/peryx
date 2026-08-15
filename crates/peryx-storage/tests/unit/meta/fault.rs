//! Deterministic redb fault injection for meta-store tests.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use redb::Database;
#[cfg(test)]
use redb::WriteTransaction;
use redb::backends::InMemoryBackend;

use super::{MetaDatabase, MetaStore};

/// Hand-written to avoid uncovered macro-generated branches.
#[derive(Debug)]
struct FaultBackend {
    inner: Arc<InMemoryBackend>,
    fault: Arc<Fault>,
}

impl redb::StorageBackend for FaultBackend {
    fn len(&self) -> io::Result<u64> {
        self.fault.pass().and_then(|()| self.inner.len())
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.fault.pass().and_then(|()| self.inner.read(offset, out))
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.fault.pass().and_then(|()| self.inner.set_len(len))
    }

    fn sync_data(&self) -> io::Result<()> {
        self.fault.pass().and_then(|()| self.inner.sync_data())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.fault.pass().and_then(|()| self.inner.write(offset, data))
    }
}

#[derive(Debug)]
pub struct Fault(AtomicI64);

impl Fault {
    const fn disabled() -> Self {
        Self(AtomicI64::new(-1))
    }

    pub fn arm(&self, after: i64) {
        self.0.store(after, Ordering::SeqCst);
    }

    pub fn disable(&self) {
        self.arm(-1);
    }

    fn pass(&self) -> io::Result<()> {
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| match remaining {
                -1 => Some(-1),
                0 => None,
                _ => Some(remaining - 1),
            })
            .map(drop)
            .map_err(|_| io::Error::other("injected storage failure"))
    }
}

fn faulted(inner: &Arc<InMemoryBackend>, fault: &Arc<Fault>) -> FaultBackend {
    FaultBackend {
        inner: inner.clone(),
        fault: fault.clone(),
    }
}

fn database(inner: &Arc<InMemoryBackend>, fault: &Arc<Fault>) -> Database {
    // A zeroed cache sends reads through the fault backend instead of cached pages.
    Database::builder()
        .set_cache_size(0)
        .create_with_backend(faulted(inner, fault))
        .unwrap()
}

pub fn backend() -> (Arc<InMemoryBackend>, Arc<Fault>) {
    (Arc::new(InMemoryBackend::new()), Arc::new(Fault::disabled()))
}

/// Fault tests must exercise production table definitions.
pub fn initialized() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>) {
    let (inner, fault) = backend();
    let store = MetaStore::initialize(database(&inner, &fault)).unwrap();
    (store, inner, fault)
}

#[cfg(test)]
pub fn create(
    inner: &Arc<InMemoryBackend>,
    fault: &Arc<Fault>,
    init: impl FnOnce(&WriteTransaction) -> Result<(), redb::TableError>,
) -> MetaStore {
    let database = database(inner, fault);
    let write = database.begin_write().unwrap();
    init(&write).unwrap();
    write.commit().unwrap();
    MetaStore {
        db: Arc::new(MetaDatabase::ReadWrite(database)),
    }
}

pub fn reopen(inner: &Arc<InMemoryBackend>, fault: &Arc<Fault>) -> MetaStore {
    MetaStore {
        db: Arc::new(MetaDatabase::ReadWrite(database(inner, fault))),
    }
}

#[cfg(test)]
pub fn corrupt(store: &MetaStore, table: redb::TableDefinition<'_, &str, &[u8]>, key: &str, bytes: &[u8]) {
    let write = store.db.begin_write().unwrap();
    write.open_table(table).unwrap().insert(key, bytes).unwrap();
    write.commit().unwrap();
}

#[test]
fn test_backend_delegates_every_call_then_faults_on_demand() {
    use redb::StorageBackend as _;

    let (inner, fault) = backend();
    let disk = faulted(&inner, &fault);
    assert!(format!("{disk:?}").contains("FaultBackend"));

    disk.set_len(64).unwrap();
    disk.write(0, b"hello").unwrap();
    disk.sync_data().unwrap();
    assert_eq!(disk.len().unwrap(), inner.len().unwrap());
    let mut buf = [0_u8; 5];
    disk.read(0, &mut buf).unwrap();
    assert_eq!(&buf, b"hello");

    fault.arm(0);
    assert!(disk.len().is_err());
    fault.disable();
    assert!(disk.len().is_ok());
}

#[test]
fn test_initialized_opens_the_production_schema() {
    let (store, _inner, _fault) = initialized();
    store.initialize_distributed_state().unwrap();

    assert!(store.view_frontiers().unwrap().is_empty());
}
