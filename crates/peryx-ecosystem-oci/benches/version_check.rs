#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/registry.rs"]
mod registry;

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_ecosystem_oci::OciRegistry;

use registry::{get, runtime, seeded};

fn bench_version_check(criterion: &mut Criterion) {
    let runtime = runtime();
    let (_dir, app, _, _) = seeded(&runtime, OciRegistry::default());
    criterion.bench_function("oci_version_check", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, "/v2/"));
    });
}

criterion_group!(benches, bench_version_check);
criterion_main!(benches);
