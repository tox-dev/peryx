//! Meta-store helpers over the shared redb fault injector, plus the tests that pin its behaviour.
//!
//! The injector itself lives in `peryx-test-support` so crates outside this one can make a store's
//! reads fail; only the helpers that need `MetaStore` internals stay here.

use std::sync::Arc;

use peryx_test_support::fault::faulted;
use redb::WriteTransaction;
use redb::backends::InMemoryBackend;
use rstest::rstest;

use super::MetaStore;

pub use peryx_test_support::fault::{Fault, backend};

/// Fault tests must exercise production table definitions.
pub fn initialized() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>) {
    let (inner, fault) = backend();
    let store = MetaStore::open_backend(faulted(&inner, &fault)).unwrap();
    (store, inner, fault)
}

pub fn reopen(inner: &Arc<InMemoryBackend>, fault: &Arc<Fault>) -> MetaStore {
    MetaStore::reopen_backend(faulted(inner, fault)).unwrap()
}

pub fn create(
    inner: &Arc<InMemoryBackend>,
    fault: &Arc<Fault>,
    init: impl FnOnce(&WriteTransaction) -> Result<(), redb::TableError>,
) -> MetaStore {
    let store = reopen(inner, fault);
    let write = store.db.begin_write().unwrap();
    init(&write).unwrap();
    write.commit().unwrap();
    store
}

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

#[rstest]
#[case::exact_budget(&[false, false], false)]
#[case::after_budget(&[false, false, true], true)]
fn test_fault_reports_injection_only_after_the_budget(
    #[case] expected_errors: &[bool],
    #[case] expected_triggered: bool,
) {
    use redb::StorageBackend as _;

    let (inner, fault) = backend();
    let disk = faulted(&inner, &fault);
    fault.arm(2);

    assert_eq!(
        (
            expected_errors.iter().map(|_| disk.len().is_err()).collect::<Vec<_>>(),
            fault.triggered(),
        ),
        (expected_errors.to_vec(), expected_triggered)
    );
}

#[test]
fn test_initialized_opens_the_production_schema() {
    let (store, _inner, _fault) = initialized();
    store.initialize_distributed_state().unwrap();

    assert!(store.view_frontiers().unwrap().is_empty());
}
