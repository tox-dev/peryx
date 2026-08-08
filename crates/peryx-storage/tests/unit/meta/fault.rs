//! A shared redb fault-injection backend for meta-store tests.
//!
//! A [`mockall`] mock [`redb::StorageBackend`] wraps an [`InMemoryBackend`] and consults a [`Fault`]
//! countdown before each operation, so a chosen backend call fails deterministically. Reopening a
//! store over the same backend with a zeroed page cache forces every read through the mock, so the
//! read-path error arms fire alongside write faults.
//!
//! Each store's fault tests open only the tables that store touches by passing an initializer to
//! [`create`]; the counterpart [`reopen`] wraps the populated backend without reinitializing it.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use redb::backends::InMemoryBackend;
use redb::{Database, StorageBackend as _, WriteTransaction};

use super::{MetaDatabase, MetaStore};

/// A redb backend that delegates to an in-memory backend until its fault is armed, then fails the
/// chosen operation. Hand-written rather than mocked so no macro-generated arm sits uncovered.
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

/// A countdown that fails the nth backend operation after it is armed.
#[derive(Debug)]
pub struct Fault(AtomicI64);

impl Fault {
    fn disabled() -> Self {
        Self(AtomicI64::new(-1))
    }

    /// Fail the operation `after` further backend calls; `0` fails the next one.
    pub fn arm(&self, after: i64) {
        self.0.store(after, Ordering::SeqCst);
    }

    /// Stop injecting failures.
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
    // A zeroed cache keeps tree pages crossing the fault boundary after it is armed, so reads reach
    // the fault backend instead of an in-process page copy.
    Database::builder()
        .set_cache_size(0)
        .create_with_backend(faulted(inner, fault))
        .unwrap()
}

/// A fresh mock backend and its disarmed fault handle.
pub fn backend() -> (Arc<InMemoryBackend>, Arc<Fault>) {
    (Arc::new(InMemoryBackend::new()), Arc::new(Fault::disabled()))
}

/// Open a store over `inner`, initializing the tables `init` opens. Keep the fault disabled here so
/// initialization and any seeding run cleanly.
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

/// Reopen a store over an already-populated `inner` without touching its tables, so an armed fault
/// reaches the cache-free read path.
pub fn reopen(inner: &Arc<InMemoryBackend>, fault: &Arc<Fault>) -> MetaStore {
    MetaStore {
        db: Arc::new(MetaDatabase::ReadWrite(database(inner, fault))),
    }
}

/// Overwrite `key` in a `<&str, &[u8]>` table with raw `bytes`, so a store's decode arm meets a
/// malformed record without a backend fault.
pub fn corrupt(store: &MetaStore, table: redb::TableDefinition<'_, &str, &[u8]>, key: &str, bytes: &[u8]) {
    let write = store.db.begin_write().unwrap();
    write.open_table(table).unwrap().insert(key, bytes).unwrap();
    write.commit().unwrap();
}

#[test]
fn test_backend_delegates_every_call_then_faults_on_demand() {
    // Drive each StorageBackend method directly: redb does not exercise all five in the store tests,
    // yet every one must delegate to the inner backend when disarmed and fail once armed.
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
