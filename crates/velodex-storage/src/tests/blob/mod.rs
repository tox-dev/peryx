use crate::blob::{BlobStore, Digest};

mod backend_tests;
mod error_tests;
mod store_tests;

pub(super) fn store() -> (tempfile::TempDir, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    (dir, store)
}

pub(super) fn collect_entries(store: &BlobStore) -> Vec<(Option<Digest>, u64)> {
    let mut entries = Vec::new();
    store
        .scan(|entry| {
            entries.push((entry.digest, entry.bytes));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    entries
}
