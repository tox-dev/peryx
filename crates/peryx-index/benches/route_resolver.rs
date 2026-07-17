use std::alloc::System;
use std::hint::black_box;
use std::time::Instant;

use hdrhistogram::Histogram;
use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind, RouteResolver, remainder};
use peryx_policy::Policy;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, Stats, StatsAlloc};

const REPOSITORY_COUNTS: [usize; 4] = [1, 32, 256, 2048];
const SAMPLES: usize = 20_000;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn main() {
    for repository_count in REPOSITORY_COUNTS {
        report(repository_count);
    }
}

fn report(repository_count: usize) {
    let indexes = indexes(repository_count);
    let path = format!("tenant-{}/pypi/simple/project", repository_count - 1);
    let build_region = Region::new(ALLOCATOR);
    let resolver = RouteResolver::new(&indexes);
    let build = build_region.change();
    let lookup_region = Region::new(ALLOCATOR);
    for _ in 0..SAMPLES {
        black_box(resolver.resolve(black_box(path.as_str())));
    }
    let lookup = lookup_region.change();
    let precomputed = latency(|| {
        black_box(resolver.resolve(black_box(path.as_str())));
    });
    let linear = latency(|| {
        black_box(linear_resolve(&indexes, black_box(path.as_str())));
    });

    println!(
        "repositories={repository_count} build_allocations={} retained_bytes={} lookup_allocations={} lookup_bytes={} precomputed_p50_ns={} precomputed_p99_ns={} linear_p50_ns={} linear_p99_ns={}",
        build.allocations,
        retained_bytes(build),
        lookup.allocations,
        lookup.bytes_allocated,
        precomputed.value_at_quantile(0.5),
        precomputed.value_at_quantile(0.99),
        linear.value_at_quantile(0.5),
        linear.value_at_quantile(0.99),
    );
}

fn indexes(count: usize) -> Vec<Index> {
    (0..count)
        .map(|position| Index {
            name: format!("repository-{position}"),
            route: format!("tenant-{position}/pypi"),
            ecosystem: Ecosystem::Pypi,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        })
        .collect()
}

fn latency(mut operation: impl FnMut()) -> Histogram<u64> {
    let mut histogram = Histogram::new(3).unwrap();
    for _ in 0..SAMPLES {
        let start = Instant::now();
        operation();
        histogram
            .record(u64::try_from(start.elapsed().as_nanos()).unwrap())
            .unwrap();
    }
    histogram
}

fn linear_resolve<'a>(indexes: &[Index], path: &'a str) -> Option<(usize, &'a str)> {
    let mut best: Option<(usize, &str)> = None;
    for (position, index) in indexes.iter().enumerate() {
        if let Some(rest) = remainder(path, &index.route)
            && best.is_none_or(|(current, _)| index.route.len() > indexes[current].route.len())
        {
            best = Some((position, rest));
        }
    }
    best
}

fn retained_bytes(stats: Stats) -> isize {
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
        + stats.bytes_reallocated
}
