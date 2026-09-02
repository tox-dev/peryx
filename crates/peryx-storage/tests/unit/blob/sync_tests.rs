use std::path::Path;

use super::{create_dir_durable, sync_parent, sync_tree};

#[test]
fn test_sync_parent_flushes_the_directory_holding_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let entry = dir.path().join("blob");
    std::fs::write(&entry, b"payload").unwrap();

    assert!(sync_parent(&entry).is_ok());
}

#[test]
fn test_sync_parent_flushes_the_working_directory_for_a_bare_name() {
    assert!(sync_parent(Path::new("blob")).is_ok());
}

#[test]
fn test_sync_parent_rejects_a_path_that_names_no_entry() {
    let error = sync_parent(Path::new("/")).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn test_sync_parent_reports_a_missing_directory_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent");

    let error = sync_parent(&absent.join("blob")).unwrap_err();

    let message = error.to_string();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        message.starts_with(&format!("flush directory {}", absent.display())),
        "{message}"
    );
}

#[test]
fn test_create_dir_durable_creates_a_whole_fan_out() {
    let dir = tempfile::tempdir().unwrap();
    let leaf = dir.path().join("sha256").join("ab").join("cd");

    create_dir_durable(&leaf).unwrap();

    assert!(leaf.is_dir());
}

#[test]
fn test_create_dir_durable_accepts_a_directory_that_already_exists() {
    let dir = tempfile::tempdir().unwrap();

    assert!(create_dir_durable(dir.path()).is_ok());
}

#[cfg(unix)]
#[test]
fn test_create_dir_durable_reports_an_unflushable_new_ancestor() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    std::fs::create_dir(&root).unwrap();
    // Writable and traversable but not readable: the fan-out is created, then refuses to be opened.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o333)).unwrap();

    let created = create_dir_durable(&root.join("sha256").join("ab").join("cd"));

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(created.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
}

#[cfg(unix)]
#[test]
fn test_create_dir_durable_reports_a_creation_failure() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();

    let created = create_dir_durable(&root.join("sha256"));

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(created.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn test_sync_tree_flushes_every_directory_below_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("blobs").join("sha256").join("ab");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("blob"), b"payload").unwrap();

    assert!(sync_tree(dir.path()).is_ok());
}

#[test]
fn test_sync_tree_names_the_directory_it_could_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent");

    let error = sync_tree(&absent).unwrap_err();

    let message = error.to_string();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        message.starts_with(&format!("read directory {}", absent.display())),
        "{message}"
    );
}
