//! CPU-bound microbenchmarks of the page transform, for `CodSpeed`'s instrumented instrument.
//!
//! The end-to-end serving benchmark in `serve.rs` drives real network and disk, so it needs the
//! walltime instrument on `CodSpeed`'s macro runners; those syscalls are exactly what the
//! instruction-counting instrument cannot see. This benchmark isolates the one stretch of the
//! serving path that is pure CPU, the streaming PEP 691 transform that rewrites every file URL to
//! the local route, so the instrumented instrument can guard it against per-commit regressions on
//! an ordinary runner, fast and free.
#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary the nursery lint flags"
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use velodex_http::stream::{PageContext, PageTransformer};

/// Real PEP 691 pages captured from pypi.org, the same fixtures the serving benchmark installs.
const PAGES: &[(&str, &str)] = &[
    ("flask", include_str!("fixtures/pages/flask.json")),
    ("requests", include_str!("fixtures/pages/requests.json")),
    ("numpy", include_str!("fixtures/pages/numpy.json")),
];

/// The size velodex streams upstream bytes in. The transform carries its lexer state across chunk
/// boundaries, so feeding it in chunks measures the real per-boundary work rather than one pass over
/// a single buffer.
const CHUNK: usize = 16 * 1024;

/// Run one page through a fresh transformer exactly as the serving path does: chunked in, rewritten
/// out, then closed.
fn transform_page(bytes: &[u8]) {
    let mut transformer = PageTransformer::new(PageContext {
        route: "root/pypi".to_owned(),
        ..PageContext::default()
    });
    for chunk in bytes.chunks(CHUNK) {
        let out = transformer.push(chunk).expect("a captured page transforms");
        std::hint::black_box(out);
    }
    transformer.finish().expect("a captured page closes cleanly");
}

fn bench_transform(c: &mut Criterion) {
    let mut group = c.benchmark_group("transform");
    for (name, page) in PAGES {
        let bytes = page.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), bytes, |b, bytes| {
            b.iter(|| transform_page(bytes));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_transform);
criterion_main!(benches);
