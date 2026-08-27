#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_storage::blob::{BlobStore, Digest};

fn bench_blob_store_read(criterion: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    let bytes = [0x7fu8; 4096];
    let digest = Digest::of(&bytes);
    store.write_verified(&bytes, &digest).unwrap();
    criterion.bench_function("oci_blob_store_read_fixed_hash", |bencher| {
        bencher.iter(|| std::hint::black_box(store.read_range(&digest, 0..4096).unwrap()));
    });
}

criterion_group!(benches, bench_blob_store_read);
criterion_main!(benches);
