#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/registry.rs"]
mod registry;

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_ecosystem_oci::OciRegistry;

use registry::{get, runtime, seeded};

fn bench_manifest_by_digest(criterion: &mut Criterion) {
    let runtime = runtime();
    let (_dir, app, manifest_digest, _) = seeded(&runtime, OciRegistry::default());
    let uri = format!("/v2/store/app/manifests/{manifest_digest}");
    criterion.bench_function("oci_manifest_by_digest", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, &uri));
    });
}

criterion_group!(benches, bench_manifest_by_digest);
criterion_main!(benches);
