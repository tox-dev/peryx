//! Per-request cost of a query that needs exact verification, against a growing corpus.
//!
//! Verification runs per candidate document rather than over the term dictionary, so the reported
//! latency should stay flat as `corpus_bytes` grows. A clause that streamed the dictionary would
//! instead grow with it.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use hdrhistogram::Histogram;
use peryx_core::LexiconRegistry;
use peryx_index::Index;
use peryx_search::{
    ContentSource, IndexerCtx, SearchCtx, SearchDocument, SearchDocumentProvider, SearchError, SearchIndex,
    SearchParams,
};
use peryx_storage::blob::{BlobStorage, BlobStore};
use peryx_storage::meta::MetaStore;

const DOCUMENT_COUNTS: [usize; 3] = [8, 32, 128];
const FILLER_BYTES: usize = 256;
const SAMPLES: usize = 50;
/// Longer than the n-gram width, so the query needs verification beyond the prefilter.
const NEEDLE: &str = "release-candidate-2026";

struct Corpus {
    size: usize,
}

impl SearchDocumentProvider for Corpus {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok((0..self.size)
            .map(|position| {
                let filler = "abcdefghijklmnopqrstuvwxyz".repeat(FILLER_BYTES / 26 + 1);
                let name = format!("pkg{position}");
                SearchDocument {
                    display_label: name.clone(),
                    resource_key: name,
                    route: "root".to_owned(),
                    index: "root".to_owned(),
                    ecosystem: "alpha".to_owned(),
                    source: ContentSource::Cached,
                    available_locally: false,
                    summary: None,
                    text: format!("{}{}", &filler[..FILLER_BYTES], if position == 0 { NEEDLE } else { "" }),
                }
            })
            .collect())
    }
}

fn main() {
    for document_count in DOCUMENT_COUNTS {
        report(document_count);
    }
}

fn report(document_count: usize) {
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

    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(Corpus { size: document_count }));
    let search = search;
    let params = SearchParams {
        query: NEEDLE.to_owned(),
        ..SearchParams::default()
    };
    assert_eq!(search.search(&ctx, params.clone()).unwrap().total, 1);

    let verified = latency(|| {
        black_box(search.search(black_box(&ctx), params.clone()).unwrap());
    });

    println!(
        "documents={document_count} corpus_bytes={} verified_p50_ns={} verified_p99_ns={}",
        document_count * FILLER_BYTES,
        verified.value_at_quantile(0.5),
        verified.value_at_quantile(0.99),
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
