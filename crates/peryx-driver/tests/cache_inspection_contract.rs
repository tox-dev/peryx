use std::error::Error as _;
use std::io::{Error, ErrorKind};

use peryx_core::Ecosystem;
use peryx_driver::DriverSet;
use peryx_driver::cache_inspection::{
    CacheInspectionError, CacheListFilter, CachePageSource, write_cache_fsck, write_cache_list, write_cache_size,
};
use peryx_driver::serving::{CachePage, CapabilityRegistrar, FsckDriver};
use peryx_identity::UserId;
use peryx_storage::blob::{BlobError, BlobScanError, BlobStorage};
use peryx_storage::meta::{MetaError, MetaStore, NewRepository};
use rstest::rstest;

#[rstest]
#[case::write(CacheInspectionError::Write(Error::new(ErrorKind::BrokenPipe, "closed")), "closed")]
#[case::page_output(
    CacheInspectionError::PageOutput(Error::new(ErrorKind::BrokenPipe, "closed")),
    "scan cached index pages"
)]
#[case::blob_scan(
    CacheInspectionError::BlobScan(BlobScanError::Visit(Error::new(ErrorKind::PermissionDenied, "denied"))),
    "scan blob files"
)]
#[case::blob_stages(
    CacheInspectionError::BlobStages(BlobError::io(Error::new(ErrorKind::PermissionDenied, "denied"))),
    "scan blob stages"
)]
#[case::repository_ecosystems(
    CacheInspectionError::RepositoryEcosystems(MetaError::DriverPrecondition("broken".to_owned())),
    "read repository ecosystems"
)]
#[case::ecosystem_fsck(
    CacheInspectionError::EcosystemFsck("cannot check metadata".to_owned()),
    "fsck ecosystem metadata: cannot check metadata"
)]
fn cache_inspection_errors_name_the_failing_step(#[case] error: CacheInspectionError, #[case] expected: &str) {
    assert_eq!(error.to_string(), expected);
}

#[rstest]
#[case::write(CacheInspectionError::Write(Error::new(ErrorKind::BrokenPipe, "closed")))]
#[case::page_output(CacheInspectionError::PageOutput(Error::new(ErrorKind::BrokenPipe, "closed")))]
#[case::blob_scan(CacheInspectionError::BlobScan(BlobScanError::Visit(Error::new(
    ErrorKind::PermissionDenied,
    "denied"
))))]
#[case::blob_stages(CacheInspectionError::BlobStages(BlobError::io(Error::new(
    ErrorKind::PermissionDenied,
    "denied"
))))]
#[case::repository_ecosystems(CacheInspectionError::RepositoryEcosystems(MetaError::DriverPrecondition(
    "broken".to_owned()
)))]
fn cache_inspection_errors_keep_their_cause(#[case] error: CacheInspectionError) {
    assert!(error.source().is_some());
}

const NOW: i64 = 1_000_000;
const TTL: i64 = 100;

fn page(index: &str, resource: &str, fetched_at_unix: i64, fresh_secs: Option<i64>, body_bytes: u64) -> CachePage {
    CachePage {
        index: index.to_owned(),
        resource: resource.to_owned(),
        fetched_at_unix,
        fresh_secs,
        body_bytes,
        record_bytes: 0,
        key: format!("{index}/{resource}"),
    }
}

fn empty_blobs() -> (tempfile::TempDir, BlobStorage) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    (dir, blobs)
}

/// Runs `write_cache_list` over one source of `pages` and returns the listed pages' keys, in the
/// order they appear, with the header row dropped. Every case below fixes every filter dimension
/// except the one it names, so a single boundary in that dimension is what decides which of the two
/// pages given to it survives.
fn listed_keys(pages: Vec<CachePage>, resource_filter: Option<&str>, filter: &CacheListFilter<'_>) -> Vec<String> {
    let (_dir, blobs) = empty_blobs();
    let mut out = Vec::new();
    write_cache_list(
        vec![CachePageSource {
            pages,
            resource_filter: resource_filter.map(str::to_owned),
        }],
        &blobs,
        filter,
        TTL,
        NOW,
        &mut out,
    )
    .unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| line.rsplit('\t').next().unwrap().to_owned())
        .collect()
}

const fn no_filter() -> CacheListFilter<'static> {
    CacheListFilter {
        index: None,
        resource_filtered: false,
        digest: None,
        stale: false,
        min_age_secs: None,
        min_size_bytes: None,
    }
}

/// An index filter keeps only the page it names: a mismatched page must not survive under the guise
/// of "not explicitly excluded", and a matching page must not be swept out by the same term.
#[test]
fn write_cache_list_index_filter_keeps_only_the_matching_page() {
    let keep = page("wanted-index", "resource", NOW, Some(TTL), 0);
    let drop = page("other-index", "resource", NOW, Some(TTL), 0);
    let filter = CacheListFilter {
        index: Some("wanted-index"),
        ..no_filter()
    };

    assert_eq!(listed_keys(vec![keep.clone(), drop], None, &filter), vec![keep.key]);
}

/// A per-source resource filter excludes a page from a different resource even though nothing else
/// asked to exclude it, and keeps the one it names.
#[test]
fn write_cache_list_resource_filter_keeps_only_the_matching_resource() {
    let keep = page("index", "wanted-resource", NOW, Some(TTL), 0);
    let drop = page("index", "other-resource", NOW, Some(TTL), 0);

    assert_eq!(
        listed_keys(vec![keep.clone(), drop], Some("wanted-resource"), &no_filter()),
        vec![keep.key]
    );
}

/// Requesting only stale entries drops a fresh page and keeps a stale one; the two pages sit exactly
/// on the freshness boundary their `fresh_secs` names, so the filter has to read `is_stale` correctly
/// rather than approximate it.
#[test]
fn write_cache_list_stale_filter_keeps_only_stale_pages() {
    let fresh = page("index", "fresh", NOW, Some(TTL), 0);
    let stale = page("index", "stale", NOW - TTL, Some(TTL), 0);
    let filter = CacheListFilter {
        stale: true,
        ..no_filter()
    };

    assert_eq!(listed_keys(vec![fresh, stale.clone()], None, &filter), vec![stale.key]);
}

/// A minimum age excludes a page younger than it and keeps one exactly at the boundary: the
/// comparison is a strict `<`, so equality must count as old enough.
#[test]
fn write_cache_list_min_age_filter_keeps_pages_at_or_past_the_boundary() {
    let young = page("index", "young", NOW - 49, Some(TTL), 0);
    let old = page("index", "old", NOW - 50, Some(TTL), 0);
    let filter = CacheListFilter {
        min_age_secs: Some(50),
        ..no_filter()
    };

    assert_eq!(listed_keys(vec![young, old.clone()], None, &filter), vec![old.key]);
}

/// A minimum size excludes a page smaller than it and keeps one exactly at the boundary, mirroring
/// the age boundary above.
#[test]
fn write_cache_list_min_size_filter_keeps_pages_at_or_past_the_boundary() {
    let small = page("index", "small", NOW, Some(TTL), 49);
    let big = page("index", "big", NOW, Some(TTL), 50);
    let filter = CacheListFilter {
        min_size_bytes: Some(50),
        ..no_filter()
    };

    assert_eq!(listed_keys(vec![small, big.clone()], None, &filter), vec![big.key]);
}

/// Any active page filter (an index here) means the request is scoped to the index pages, so the
/// blob scan that would otherwise follow must not run and add rows the caller never asked to see.
#[test]
fn write_cache_list_skips_the_blob_scan_once_any_page_filter_is_active() {
    let (_dir, blobs) = empty_blobs();
    blobs.blocking().put_bytes(b"unwanted blob").unwrap();
    let filter = CacheListFilter {
        index: Some("index"),
        ..no_filter()
    };
    let mut out = Vec::new();

    write_cache_list(Vec::new(), &blobs, &filter, TTL, NOW, &mut out).unwrap();

    assert_eq!(
        String::from_utf8(out).unwrap(),
        "kind\tindex\tresource\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"
    );
}

/// With no page filter active, the blob scan runs, and a digest filter on it keeps only the blob it
/// names.
#[test]
fn write_cache_list_blob_digest_filter_keeps_only_the_matching_digest() {
    let (_dir, blobs) = empty_blobs();
    let wanted = blobs.blocking().put_bytes(b"wanted blob contents").unwrap();
    blobs.blocking().put_bytes(b"other blob contents").unwrap();
    let filter = CacheListFilter {
        digest: Some(wanted.as_str()),
        ..no_filter()
    };
    let mut out = Vec::new();

    write_cache_list(Vec::new(), &blobs, &filter, TTL, NOW, &mut out).unwrap();

    let output = String::from_utf8(out).unwrap();
    let blob_lines = output.lines().skip(1).count();
    assert_eq!((blob_lines, output.contains(wanted.as_str())), (1, true));
}

/// A blob-side minimum size excludes a blob smaller than it and keeps one exactly at the boundary,
/// same as the index-page minimum size above.
#[test]
fn write_cache_list_blob_min_size_filter_keeps_blobs_at_or_past_the_boundary() {
    let (_dir, blobs) = empty_blobs();
    let small = blobs.blocking().put_bytes(&[0_u8; 49]).unwrap();
    let big = blobs.blocking().put_bytes(&[1_u8; 50]).unwrap();
    let filter = CacheListFilter {
        min_size_bytes: Some(50),
        ..no_filter()
    };
    let mut out = Vec::new();

    write_cache_list(Vec::new(), &blobs, &filter, TTL, NOW, &mut out).unwrap();

    let output = String::from_utf8(out).unwrap();
    assert_eq!(
        (output.contains(small.as_str()), output.contains(big.as_str())),
        (false, true)
    );
}

/// Index-page accounting (byte totals and the stale count) and blob accounting (file count, byte
/// total, and invalid-path count) are each a running sum, not a single overwrite: two pages and
/// three blob-directory entries, none of them alone equal to the total, are what make an
/// accumulation bug visible instead of coincidentally matching.
#[test]
fn write_cache_size_accumulates_every_counter_across_multiple_entries() {
    let (dir, blobs) = empty_blobs();
    blobs.blocking().put_bytes(&[0_u8; 3]).unwrap();
    blobs.blocking().put_bytes(&[1_u8; 11]).unwrap();
    let stray = dir.path().join("blobs/sha256/zz/zz");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("not-a-digest"), [2_u8; 7]).unwrap();
    let pages = [
        CachePage {
            record_bytes: 5,
            ..page("index", "a", NOW, Some(-1), 0)
        },
        CachePage {
            record_bytes: 7,
            ..page("index", "b", NOW - 10, Some(10), 0)
        },
    ];
    let mut out = Vec::new();

    write_cache_size(&pages, Vec::new(), &blobs, TTL, NOW, &mut out).unwrap();

    assert_eq!(
        String::from_utf8(out).unwrap(),
        "index_pages\t2\n\
         stale_index_pages\t2\n\
         index_bytes\t12\n\
         blob_files\t3\n\
         blob_bytes\t21\n\
         invalid_blob_paths\t1\n\
         stage_files\t0\n\
         stage_bytes\t0\n"
    );
}

struct FsckStub {
    problems: u64,
}

impl FsckDriver for FsckStub {
    fn fsck_metadata(
        &self,
        _meta: &MetaStore,
        _blobs: &BlobStorage,
        _indexes: &[peryx_driver::Index],
        out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        for problem in 0..self.problems {
            writeln!(out, "metadata\talpha\tproblem {problem}").map_err(|error| error.to_string())?;
        }
        Ok(self.problems)
    }
}

/// `fsck` sums problems from three independent sources - repositories with no registered checker,
/// a checker's own findings, and damaged blob entries - into one running total that gates the
/// trailing "ok" versus "problems" line. Each source alone would leave the sum indistinguishable
/// from an overwrite; together, only correct accumulation reaches the total this test expects.
#[test]
fn write_cache_fsck_reports_uncovered_ecosystems_and_sums_every_problem_source() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    for (route, ecosystem) in [("a", "alpha"), ("b", "beta")] {
        meta.create_repository(
            NewRepository {
                route: route.to_owned(),
                display_name: route.to_owned(),
                ecosystem: ecosystem.to_owned(),
                definition: serde_json::json!({}),
                created_by: UserId::random(),
            },
            1,
        )
        .unwrap();
    }
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    blobs.blocking().put_bytes(b"verifiable contents").unwrap();
    let stray = dir.path().join("blobs/sha256/zz/zz");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("not-a-digest"), b"junk").unwrap();
    let mut drivers = DriverSet::default();
    drivers.register_fsck(Ecosystem::new("alpha"), std::sync::Arc::new(FsckStub { problems: 3 }));
    let mut out = Vec::new();

    write_cache_fsck(&drivers, &meta, &blobs, &[], &mut out).unwrap();

    let output = String::from_utf8(out).unwrap();
    let lines = output.lines().collect::<Vec<_>>();
    assert!(lines.contains(&"metadata\tbeta\tmissing checker"));
    assert!(!output.contains("alpha\tmissing checker"));
    assert!(lines.contains(&"metadata\talpha\tproblem 0"));
    assert!(output.contains("invalid content-addressed path"));
    assert_eq!(lines.last(), Some(&"problems\t5"));
}

struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(Error::new(ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A checker reports its own findings through the same writer `write_cache_fsck` hands it, so a
/// write failure inside a checker's own loop must surface as an `EcosystemFsck` error rather than
/// vanish, the same as one in `write_cache_fsck`'s own writes does.
#[test]
fn write_cache_fsck_reports_a_checkers_own_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut drivers = DriverSet::default();
    drivers.register_fsck(Ecosystem::new("alpha"), std::sync::Arc::new(FsckStub { problems: 1 }));

    let error = write_cache_fsck(&drivers, &meta, &blobs, &[], &mut FailingWriter).unwrap_err();

    assert!(matches!(error, CacheInspectionError::EcosystemFsck(_)), "{error}");
    std::io::Write::flush(&mut FailingWriter).unwrap();
}
