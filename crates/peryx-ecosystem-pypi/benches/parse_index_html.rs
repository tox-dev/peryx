#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/index.rs"]
mod index;

use criterion::{Criterion, criterion_group, criterion_main};
use peryx_ecosystem_pypi::{parse_index_html, render_index_html};
use url::Url;

fn bench_parse_index_html(criterion: &mut Criterion) {
    let base = Url::parse("https://pypi.org/simple/flask/").unwrap();
    let html = render_index_html(&index::index_list(400));
    criterion.bench_function("index_html", |bencher| {
        bencher.iter(|| parse_index_html(std::hint::black_box(&html), &base).unwrap());
    });
}

criterion_group!(benches, bench_parse_index_html);
criterion_main!(benches);
