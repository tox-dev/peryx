use std::path::Path;

use super::{publish, sibling_path, staging_path, sync_parent, sync_tree};

#[test]
fn test_publish_restores_the_prior_target_when_the_swap_fails() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("data");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("marker"), b"old").unwrap();
    let missing_staging = staging_path(&target).unwrap();

    let err = publish(&missing_staging, &target).unwrap_err();

    assert!(err.to_string().contains("publish restored data"), "{err}");
    assert_eq!(std::fs::read(target.join("marker")).unwrap(), b"old");
    assert!(!root.path().join("data.restore-old").exists());
}

#[test]
fn test_sync_tree_tolerates_a_missing_directory() {
    sync_tree(Path::new("/peryx/no/such/staging"));
}

#[test]
fn test_sync_parent_of_a_parentless_path_is_a_noop() {
    sync_parent(Path::new("/"));
}

#[test]
fn test_sibling_path_reports_a_parentless_target() {
    let err = sibling_path(Path::new("/"), ".restore-staging").unwrap_err();

    assert!(err.to_string().contains("no final path component"), "{err}");
}
