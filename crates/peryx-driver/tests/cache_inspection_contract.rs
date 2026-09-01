use std::error::Error as _;
use std::io::{Error, ErrorKind};

use peryx_driver::cache_inspection::CacheInspectionError;
use peryx_storage::blob::{BlobError, BlobScanError};
use peryx_storage::meta::MetaError;
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
