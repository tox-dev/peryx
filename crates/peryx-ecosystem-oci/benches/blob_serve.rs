#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/registry.rs"]
mod registry;

use std::collections::hash_map::DefaultHasher;
use std::hash::BuildHasherDefault;

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_ecosystem_oci::OciRegistryWithHasher;

use registry::{get, runtime, seeded};

fn bench_blob_serve(criterion: &mut Criterion) {
    let runtime = runtime();
    let (_dir, app, _, blob_digest) = seeded(
        &runtime,
        OciRegistryWithHasher::<BuildHasherDefault<DefaultHasher>>::default(),
    );
    let uri = format!("/v2/store/app/blobs/{blob_digest}");
    // Random-seeded results are not a valid baseline for this fixed layout.
    criterion.bench_function("oci_blob_serve_fixed_hash", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, &uri));
    });
}

criterion_group!(benches, bench_blob_serve);
criterion_main!(benches);
