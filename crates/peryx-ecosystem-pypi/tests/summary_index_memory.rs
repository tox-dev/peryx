//! A summary must cost the same on a long history as on a short one.
//!
//! Counting projects and uploads by walking their rows, and finding the newest few by decoding every
//! record, made a status request scale with everything an index had ever published. The allocator is
//! global, so this binary runs one test.

use std::alloc::System;
use std::collections::BTreeMap;

use peryx_ecosystem_pypi::store::{Guard, PromotedRelease, PypiStore as _};
use peryx_storage::meta::{MetaError, MetaStore};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

/// Two histories far enough apart that a per-row cost cannot hide in the noise.
const SHORT_HISTORY: u32 = 100;
const LONG_HISTORY: u32 = 20_000;
/// The recent list the status contract caps at, and the only rows a summary should decode.
const RECENT_LIMIT: usize = 5;
/// One count row and five recent rows, with room for the map and the strings they carry.
const MAX_SUMMARY_BYTES: usize = 8 << 10;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_summarizing_a_long_history_costs_what_a_short_one_costs() {
    let short = store(SHORT_HISTORY);
    let long = store(LONG_HISTORY);

    let region = Region::new(ALLOCATOR);
    let short_summary = short.1.summarize_indexes(&["hosted".to_owned()], RECENT_LIMIT).unwrap();
    let short_bytes = region.change().bytes_allocated;

    let region = Region::new(ALLOCATOR);
    let long_summary = long.1.summarize_indexes(&["hosted".to_owned()], RECENT_LIMIT).unwrap();
    let long_bytes = region.change().bytes_allocated;

    assert_eq!(short_summary["hosted"].write_count, u64::from(SHORT_HISTORY));
    assert_eq!(long_summary["hosted"].write_count, u64::from(LONG_HISTORY));
    assert_eq!(long_summary["hosted"].recent_writes.len(), RECENT_LIMIT);
    assert!(short_bytes < MAX_SUMMARY_BYTES);
    assert!(long_bytes < MAX_SUMMARY_BYTES);
}

/// A store holding `uploads` published files on one index, written in one transaction so the fixture
/// costs one commit rather than one per file.
fn store(uploads: u32) -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let records: Vec<_> = (0..uploads)
        .map(|rank| {
            let filename = format!("flask-{rank:06}.whl");
            let record = format!(
                r#"{{"version":"{rank}.0","file":{{"filename":"{filename}","upload-time":"{}","size":1024}}}}"#,
                upload_time(rank)
            );
            (filename, format!("{rank:064}"), record.into_bytes())
        })
        .collect();
    meta.promote_files_checked::<MetaError>(
        false,
        &PromotedRelease {
            source: "staging",
            index: "hosted",
            normalized: "flask",
            display: "Flask",
            records: &records,
            blob_sizes: &BTreeMap::new(),
            reservations: &BTreeMap::new(),
            submitted_at_unix: 0,
        },
        |_filename, _token, _stored| Ok(Guard::Commit),
    )
    .unwrap();
    (dir, meta)
}

/// One second apart, so the newest few are a strict suffix of the history rather than a tie group.
fn upload_time(rank: u32) -> String {
    time::OffsetDateTime::from_unix_timestamp(1_767_225_600 + i64::from(rank))
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}
