//! One mutation's refresh cost against increasing repository cardinality.
//!
//! A single package mutation used to invalidate the whole derived index, so the next search re-derived
//! and rewrote every document; the cost grew with the corpus. The incremental path re-derives only the
//! mutated project, so its cost is independent of the corpus. This bench reports both against a growing
//! corpus so the difference is visible: `full_*` grows with `repositories`, `incremental_*` stays flat.

use std::alloc::System;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use hdrhistogram::Histogram;
use peryx_core::LexiconRegistry;
use peryx_index::Index;
use peryx_search::{
    IndexerCtx, PackageDocument, PackageIndexer, PackageSearch, PackageSource, ProjectUpdate, SearchCtx, SearchError,
    SearchParams, project_key,
};
use peryx_storage::blob::{BlobStorage, BlobStore};
use peryx_storage::meta::MetaStore;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, Stats, StatsAlloc};

const REPOSITORY_COUNTS: [usize; 4] = [1, 64, 512, 4096];
const SAMPLES: usize = 200;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// A corpus of `size` projects: the whole set for a full build, a single project for an incremental one.
struct Corpus {
    size: usize,
}

impl PackageIndexer for Corpus {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok((0..self.size)
            .map(|position| document(&format!("pkg{position}")))
            .collect())
    }

    fn project_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ProjectUpdate, SearchError> {
        Ok(ProjectUpdate {
            keys: vec![project_key("root", name)],
            documents: vec![document(name)],
        })
    }
}

fn document(name: &str) -> PackageDocument {
    PackageDocument {
        display_name: name.to_owned(),
        normalized_name: name.to_owned(),
        route: "root".to_owned(),
        index: "root".to_owned(),
        ecosystem: "alpha".to_owned(),
        source: PackageSource::Cached,
        available_locally: false,
        summary: None,
        text: name.to_owned(),
    }
}

fn main() {
    for repository_count in REPOSITORY_COUNTS {
        report(repository_count);
    }
}

fn report(repository_count: usize) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs: BlobStorage = BlobStore::new(dir.path().join("blobs")).into();
    let indexes: Vec<Index> = Vec::new();
    let lexicons = LexiconRegistry::default();
    let ctx = SearchCtx {
        indexer: IndexerCtx {
            indexes: &indexes,
            meta: &meta,
            blobs: &blobs,
        },
        lexicons: &lexicons,
    };

    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(Corpus { size: repository_count }));
    let search = search;
    let build_region = Region::new(ALLOCATOR);
    search.search(&ctx, SearchParams::default()).unwrap();
    let build = build_region.change();

    let full = latency(|| {
        search.bump_epoch();
        black_box(search.search(black_box(&ctx), SearchParams::default()).unwrap());
    });
    let incremental = latency(|| {
        search.invalidate_project("pkg0");
        black_box(search.search(black_box(&ctx), SearchParams::default()).unwrap());
    });

    println!(
        "repositories={repository_count} build_allocations={} retained_bytes={} full_p50_ns={} full_p99_ns={} incremental_p50_ns={} incremental_p99_ns={}",
        build.allocations,
        retained_bytes(build),
        full.value_at_quantile(0.5),
        full.value_at_quantile(0.99),
        incremental.value_at_quantile(0.5),
        incremental.value_at_quantile(0.99),
    );
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

fn retained_bytes(stats: Stats) -> isize {
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
        + stats.bytes_reallocated
}
