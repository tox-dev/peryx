use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::stage::STAGE_MAX_AGE;
use super::store::stage_file;
use super::{BlobErrorKind, BlobStorage, BlobStore, Digest, S3Config, S3Settings, StageUsage};

fn store() -> (tempfile::TempDir, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    (dir, store)
}

fn stage(directory: &Path, bytes: &[u8]) -> PathBuf {
    std::fs::create_dir_all(directory).unwrap();
    let (mut file, path) = stage_file(directory).unwrap().into_parts();
    file.write_all(bytes).unwrap();
    path.keep().unwrap()
}

/// Backdating past the age bound is what makes a stage look abandoned rather than in flight.
fn age(path: &Path) {
    let aged = SystemTime::now() - STAGE_MAX_AGE - Duration::from_secs(1);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(aged))
        .unwrap();
}

fn abandoned_stage(directory: &Path, bytes: &[u8]) -> PathBuf {
    let path = stage(directory, bytes);
    age(&path);
    path
}

fn fan_out(store: &BlobStore, digest: &Digest) -> PathBuf {
    store.path_for(digest).parent().unwrap().to_owned()
}

#[test]
fn test_sweep_removes_an_abandoned_root_stage() {
    let (_dir, store) = store();
    let path = abandoned_stage(&store.staging_dir(), b"interrupted");

    assert_eq!(store.sweep_stages().unwrap(), 1);
    assert!(!path.exists());
}

#[test]
fn test_sweep_removes_an_abandoned_fan_out_stage() {
    let (_dir, store) = store();
    let path = abandoned_stage(&fan_out(&store, &Digest::of(b"interrupted")), b"interrupted");

    assert_eq!(store.sweep_stages().unwrap(), 1);
    assert!(!path.exists());
}

#[test]
fn test_sweep_keeps_a_stage_an_in_flight_write_owns() {
    let (_dir, store) = store();
    let pending = store.begin().unwrap();
    age(pending.path());

    assert_eq!(store.sweep_stages().unwrap(), 0);
    assert!(pending.path().exists());
}

#[test]
fn test_sweep_keeps_a_stage_younger_than_the_age_bound() {
    let (_dir, store) = store();
    let path = stage(&store.staging_dir(), b"still streaming");

    assert_eq!(store.sweep_stages().unwrap(), 0);
    assert!(path.exists());
}

#[test]
fn test_sweep_leaves_files_that_are_not_stages() {
    let (_dir, store) = store();
    let digest = store.write(b"resident").unwrap();
    store.stage_upload_chunk("session", 0, b"resumable").unwrap();
    let lease = store.lease_dir().join(".peryx-lease-kept");
    std::fs::create_dir_all(store.lease_dir()).unwrap();
    std::fs::write(&lease, b"lease").unwrap();

    assert_eq!(store.sweep_stages().unwrap(), 0);
    assert!(store.exists(&digest));
    assert_eq!(store.staged_upload_len("session").unwrap(), Some(9));
    assert!(lease.exists());
}

#[test]
fn test_sweep_reports_nothing_for_a_store_that_was_never_created() {
    let (_dir, store) = store();

    assert_eq!(store.sweep_stages().unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn test_sweep_retains_a_stage_it_cannot_remove() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_dir, store) = store();
    let directory = fan_out(&store, &Digest::of(b"stranded"));
    let path = abandoned_stage(&directory, b"stranded");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();

    let swept = store.sweep_stages();

    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(swept.unwrap(), 0);
    assert!(path.exists());
}

#[cfg(unix)]
#[test]
fn test_sweep_propagates_an_unreadable_store() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_dir, store) = store();
    let root = store.staging_dir();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();

    let swept = store.sweep_stages();

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(swept.unwrap_err().kind(), BlobErrorKind::Io);
}

#[test]
fn test_stage_usage_counts_root_and_fan_out_stages() {
    let (_dir, store) = store();
    store.write(b"resident").unwrap();
    stage(&store.staging_dir(), b"root stage");
    stage(&fan_out(&store, &Digest::of(b"payload")), b"fan-out");

    assert_eq!(store.stage_usage().unwrap(), StageUsage { files: 2, bytes: 17 });
}

#[test]
fn test_scan_leaves_stage_files_out_of_the_content_addressed_entries() {
    let (_dir, store) = store();
    let digest = store.write(b"resident").unwrap();
    stage(&fan_out(&store, &digest), b"fan-out");

    let mut entries = Vec::new();
    store
        .scan(|entry| {
            entries.push(entry.digest);
            Ok::<(), std::io::Error>(())
        })
        .unwrap();

    assert_eq!(entries, vec![Some(digest)]);
}

/// A staging directory that no reachable endpoint backs: the sweep is local, so no request is made.
fn s3(staging: &Path) -> BlobStorage {
    let settings = S3Settings {
        endpoint: "http://127.0.0.1:1".to_owned(),
        bucket: "bucket".to_owned(),
        prefix: "cache".to_owned(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: Duration::from_secs(5),
        max_retries: 0,
        multipart_threshold: 5 << 20,
        part_size: 5 << 20,
        upload_concurrency: 2,
        conditional_writes: true,
        checksum_writes: true,
    };
    BlobStorage::s3(S3Config::new(settings).unwrap(), staging.to_owned())
}

#[tokio::test]
async fn test_startup_recovery_sweeps_an_abandoned_filesystem_stage() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let path = abandoned_stage(dir.path(), b"interrupted");

    assert_eq!(storage.recover_incomplete_uploads().await.unwrap(), 1);
    assert!(!path.exists());
}

#[tokio::test]
async fn test_startup_recovery_sweeps_the_s3_staging_directory() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("blob-staging");
    let storage = s3(&staging);
    let path = abandoned_stage(&staging, b"materialized");

    assert_eq!(storage.recover_incomplete_uploads().await.unwrap(), 1);
    assert!(!path.exists());
}

#[test]
fn test_stage_usage_is_unsupported_on_the_object_store() {
    let dir = tempfile::tempdir().unwrap();

    let error = s3(&dir.path().join("blob-staging"))
        .blocking()
        .stage_usage()
        .unwrap_err();

    assert_eq!(error.kind(), BlobErrorKind::Unsupported);
}

#[test]
fn test_stage_usage_counts_nothing_for_a_store_that_was_never_created() {
    let dir = tempfile::tempdir().unwrap();

    let usage = BlobStorage::filesystem(dir.path().join("blobs"))
        .blocking()
        .stage_usage()
        .unwrap();

    assert_eq!(usage, StageUsage::default());
}
