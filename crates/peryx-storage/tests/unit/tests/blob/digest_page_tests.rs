use std::num::NonZeroUsize;
use std::path::Path;

use crate::blob::{BlobDigestPage, BlobErrorKind, BlobStorage, BlobStore, Digest, S3Config, S3Settings};

/// Padding a short prefix keeps the two directory levels and the file name of each seeded blob under
/// the test's control, which `Digest::of` on arbitrary bytes would not give.
fn hex(prefix: &str) -> String {
    format!("{prefix}{}", "0".repeat(64 - prefix.len()))
}

fn digest(hex: &str) -> Digest {
    Digest::from_hex(hex).unwrap()
}

fn limit(rows: usize) -> NonZeroUsize {
    NonZeroUsize::new(rows).unwrap()
}

fn seed(root: &Path, name: &str) {
    let directory = root.join("sha256").join(&name[0..2]).join(&name[2..4]);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(name), b"body").unwrap();
}

/// Five digests spanning two first-level directories, two second-level directories and a shared leaf.
fn spread() -> (tempfile::TempDir, BlobStore, Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let names = ["0000", "00001", "0011", "aa00", "aabb"].map(hex).to_vec();
    for name in &names {
        seed(directory.path(), name);
    }
    let store = BlobStore::new(directory.path());
    (directory, store, names)
}

#[test]
fn test_a_store_without_a_blob_root_pages_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let store = BlobStore::new(directory.path());

    assert_eq!(store.scan_page(None, limit(2)).unwrap(), BlobDigestPage::default());
}

#[test]
fn test_a_page_stops_at_the_limit_and_names_its_last_digest() {
    let (_directory, store, names) = spread();

    assert_eq!(
        store.scan_page(None, limit(2)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&names[0]), digest(&names[1])],
            next_cursor: Some(names[1].clone()),
        }
    );
}

#[test]
fn test_the_cursor_resumes_at_the_following_digest() {
    let (_directory, store, names) = spread();

    assert_eq!(
        store.scan_page(Some(&names[1]), limit(2)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&names[2]), digest(&names[3])],
            next_cursor: Some(names[3].clone()),
        }
    );
}

#[test]
fn test_the_last_page_omits_the_cursor() {
    let (_directory, store, names) = spread();

    assert_eq!(
        store.scan_page(Some(&names[3]), limit(2)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&names[4])],
            next_cursor: None,
        }
    );
}

#[test]
fn test_a_page_that_exhausts_the_store_at_the_limit_omits_the_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let names = ["0000", "0011"].map(hex);
    for name in &names {
        seed(directory.path(), name);
    }
    let store = BlobStore::new(directory.path());

    assert_eq!(
        store.scan_page(None, limit(2)).unwrap(),
        BlobDigestPage {
            digests: names.iter().map(|name| digest(name)).collect(),
            next_cursor: None,
        }
    );
}

#[test]
fn test_deleting_the_cursor_digest_does_not_block_progress() {
    let (directory, store, names) = spread();
    std::fs::remove_file(directory.path().join("sha256").join("00").join("11").join(&names[2])).unwrap();

    assert_eq!(
        store.scan_page(Some(&names[2]), limit(2)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&names[3]), digest(&names[4])],
            next_cursor: None,
        }
    );
}

#[test]
fn test_a_leaf_entry_that_is_not_a_digest_is_left_out() {
    let directory = tempfile::tempdir().unwrap();
    let name = hex("0000");
    seed(directory.path(), &name);
    std::fs::write(
        directory.path().join("sha256").join("00").join("00").join("readme"),
        b"note",
    )
    .unwrap();
    let store = BlobStore::new(directory.path());

    assert_eq!(
        store.scan_page(None, limit(2)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&name)],
            next_cursor: None,
        }
    );
}

#[test]
fn test_stray_files_beside_the_prefix_directories_are_left_out() {
    let directory = tempfile::tempdir().unwrap();
    let name = hex("0000");
    seed(directory.path(), &name);
    std::fs::write(directory.path().join("sha256").join("zz"), b"note").unwrap();
    std::fs::write(directory.path().join("sha256").join("00").join("zz"), b"note").unwrap();
    let store = BlobStore::new(directory.path());

    assert_eq!(
        store.scan_page(None, limit(2)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&name)],
            next_cursor: None,
        }
    );
}

#[test]
fn test_a_cursor_skips_the_directories_it_has_already_covered() {
    let (_directory, store, names) = spread();

    assert_eq!(
        store.scan_page(Some(&names[2]), limit(5)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&names[3]), digest(&names[4])],
            next_cursor: None,
        }
    );
}

#[test]
fn test_a_missing_prefix_directory_surfaces_the_listing_error() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("sha256"), b"not a directory").unwrap();
    let store = BlobStore::new(directory.path());

    assert!(store.scan_page(None, limit(2)).is_err());
}

#[test]
fn test_a_digest_inserted_below_the_cursor_waits_for_the_next_cycle() {
    let directory = tempfile::tempdir().unwrap();
    let names = ["0011", "aa00"].map(hex);
    for name in &names {
        seed(directory.path(), name);
    }
    let store = BlobStore::new(directory.path());
    let first = store.scan_page(None, limit(1)).unwrap();
    let late = hex("0000");
    seed(directory.path(), &late);

    let rest = store.scan_page(first.next_cursor.as_deref(), limit(2)).unwrap();

    assert_eq!(
        (first, rest),
        (
            BlobDigestPage {
                digests: vec![digest(&names[0])],
                next_cursor: Some(names[0].clone()),
            },
            BlobDigestPage {
                digests: vec![digest(&names[1])],
                next_cursor: None,
            }
        )
    );
}

#[test]
fn test_a_filesystem_backend_pages_through_the_blocking_facade() {
    let directory = tempfile::tempdir().unwrap();
    let name = hex("0000");
    seed(directory.path(), &name);
    let storage = BlobStorage::filesystem(directory.path());

    assert_eq!(
        storage.blocking().digest_page(None, limit(2)).unwrap(),
        BlobDigestPage {
            digests: vec![digest(&name)],
            next_cursor: None,
        }
    );
}

#[test]
fn test_an_object_store_backend_cannot_page_blocking() {
    let directory = tempfile::tempdir().unwrap();
    let settings = S3Settings {
        endpoint: "http://127.0.0.1:1".to_owned(),
        bucket: "bucket".to_owned(),
        prefix: String::new(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: std::time::Duration::from_secs(1),
        max_retries: 0,
        multipart_threshold: 5 << 20,
        part_size: 5 << 20,
        upload_concurrency: 1,
        conditional_writes: true,
        checksum_writes: true,
    };
    let storage = BlobStorage::s3(S3Config::new(settings).unwrap(), directory.path().to_owned());

    assert_eq!(
        storage.blocking().digest_page(None, limit(2)).unwrap_err().kind(),
        BlobErrorKind::Unsupported
    );
}
