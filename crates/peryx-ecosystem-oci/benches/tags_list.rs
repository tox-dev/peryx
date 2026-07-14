#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/registry.rs"]
mod registry;

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_ecosystem_oci::OciRegistry;

use registry::{get, runtime, seeded};

fn bench_tags_list(criterion: &mut Criterion) {
    let runtime = runtime();
    let (_dir, app, _, _) = seeded(&runtime, OciRegistry::default());
    criterion.bench_function("oci_tags_list", |bencher| {
        bencher.to_async(&runtime).iter(|| get(&app, "/v2/store/app/tags/list"));
    });
}

criterion_group!(benches, bench_tags_list);
criterion_main!(benches);
