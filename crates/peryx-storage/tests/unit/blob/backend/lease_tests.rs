use std::io::ErrorKind;

use super::BlobLease;

#[test]
fn test_pinned_copies_when_hard_links_are_unavailable() {
    let source_root = tempfile::tempdir().unwrap();
    let lease_root = tempfile::tempdir().unwrap();
    let source = source_root.path().join("blob");
    std::fs::write(&source, b"payload").unwrap();

    let lease = BlobLease::pinned_with(&source, lease_root.path(), &|_, _| {
        Err(std::io::Error::from(ErrorKind::Unsupported))
    })
    .unwrap();

    assert_eq!(std::fs::read(lease.path()).unwrap(), b"payload");
}
